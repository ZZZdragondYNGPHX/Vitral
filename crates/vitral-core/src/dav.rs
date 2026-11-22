use crate::clip::ClipboardEntry;
use crate::eng::EngineConfig;
use crate::error::VitralError;
use crate::imp::LexiconImportArtifact;
use crate::offline_guard::OfflineGuard;
use crate::ret::RetentionCrdtDelta;
use crate::sync::{
    base64_decode, base64_encode, decode_clipboard_ring_jsonl, decrypt_envelope,
    encode_clipboard_ring_jsonl, encrypt_envelope, sha256_hex, zstd_compress, zstd_decompress,
    CleanupResult, LexiconSnapshotPayload, ProfileBackup, RelayPollResult, RelayReceivedItem,
    RelaySendResult, RestoreResult,
};
use chrono::{DateTime, SecondsFormat, Utc};
use opendal::layers::TimeoutLayer;
use opendal::{blocking, HttpTransporter, Operator};
use opendal_http_transport_reqwest::ReqwestTransport;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
use vitral_tokens::TOKENS;

static DAV_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static TLS_CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();

fn dav_runtime() -> Result<&'static tokio::runtime::Runtime, VitralError> {
    if let Some(rt) = DAV_RUNTIME.get() {
        return Ok(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| VitralError::Internal(format!("tokio runtime init failed: {e}")))?;
    let _ = DAV_RUNTIME.set(rt);
    DAV_RUNTIME
        .get()
        .ok_or_else(|| VitralError::Internal("tokio runtime missing after init".into()))
}

fn rustls_client_config() -> &'static rustls::ClientConfig {
    TLS_CONFIG.get_or_init(|| {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("valid TLS protocol versions")
            .with_root_certificates(root_store)
            .with_no_client_auth()
    })
}

fn run_on_blocking_dav_thread<T, F>(f: F) -> Result<T, VitralError>
where
    T: Send,
    F: FnOnce() -> Result<T, VitralError> + Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .name("vitral-dav-blocking".to_string())
                .spawn_scoped(s, f)
                .map_err(|e| VitralError::Io(format!("spawn dav blocking thread failed: {e}")))?
                .join()
                .map_err(|_| VitralError::Internal("dav blocking thread panicked".into()))?
        })
    } else {
        f()
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct DavConfig {
    pub endpoint: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub root: Option<String>,
    pub sync_key: Option<String>,
}

impl std::fmt::Debug for DavConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DavConfig")
            .field("endpoint", &self.endpoint)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("root", &self.root)
            .field("sync_key", &self.sync_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl DavConfig {
    pub fn from_engine_config(cfg: &EngineConfig) -> Option<Self> {
        let ep = cfg.koofr_endpoint.as_deref()?.trim();
        if ep.is_empty() {
            return None;
        }
        Some(Self {
            endpoint: ep.to_string(),
            username: cfg
                .koofr_username
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            password: cfg
                .koofr_password
                .as_deref()
                .map(|s| s.trim_matches(['\r', '\n']))
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            root: cfg.koofr_root.clone(),
            sync_key: cfg.sync_key.clone(),
        })
    }

    pub fn from_json_with_fallback(
        cfg_json: &str,
        fallback: Option<&DavConfig>,
    ) -> Result<Self, VitralError> {
        if cfg_json.trim().is_empty() {
            return Err(VitralError::InvalidArgument(
                "webdav_cfg_json cannot be empty".into(),
            ));
        }
        #[derive(Debug, Deserialize)]
        struct RawDavCfg {
            #[serde(default, alias = "webdav_url")]
            koofr_endpoint: Option<String>,
            #[serde(default)]
            koofr_username: Option<String>,
            #[serde(default)]
            koofr_password: Option<String>,
            #[serde(default)]
            koofr_root: Option<String>,
            #[serde(default)]
            sync_key: Option<String>,
        }
        let mut value: serde_json::Value = serde_json::from_str(cfg_json).map_err(|e| VitralError::Parse(e.to_string()))?;
        crate::sync::merge_alias(&mut value,"koofr_endpoint","webdav_url")?;
        let parsed: RawDavCfg = serde_json::from_value(value)
            .map_err(|e| VitralError::Parse(format!("invalid webdav_cfg_json: {e}")))?;
        let endpoint = parsed
            .koofr_endpoint
            .filter(|s| !s.trim().is_empty())
            .or_else(|| fallback.map(|f| f.endpoint.clone()))
            .unwrap_or_default();
        if endpoint.trim().is_empty() {
            return Err(VitralError::InvalidArgument(
                "koofr_endpoint cannot be empty".into(),
            ));
        }
        Ok(Self {
            endpoint: endpoint.trim().to_string(),
            username: parsed
                .koofr_username
                .or_else(|| fallback.and_then(|f| f.username.clone()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            password: parsed
                .koofr_password
                .or_else(|| fallback.and_then(|f| f.password.clone()))
                .map(|s| s.trim_matches(['\r', '\n']).to_string())
                .filter(|s| !s.is_empty()),
            root: parsed
                .koofr_root
                .or_else(|| fallback.and_then(|f| f.root.clone())),
            sync_key: parsed
                .sync_key
                .or_else(|| fallback.and_then(|f| f.sync_key.clone())),
        })
    }
}

fn validate_safe_id(label: &str, value: &str) -> Result<(), VitralError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains("..")
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(VitralError::InvalidArgument(format!(
            "invalid {label}: must match [A-Za-z0-9._-] without traversal segments"
        )));
    }
    Ok(())
}

fn validate_rel_path(rel_path: &str, allow_empty: bool) -> Result<String, VitralError> {
    if rel_path.contains('\0') || rel_path.contains('\\') {
        return Err(VitralError::InvalidArgument(
            "invalid WebDAV path: contains NUL or backslash".into(),
        ));
    }
    let trimmed = rel_path.trim_start_matches('/');
    for seg in trimmed.split('/') {
        if seg == "." || seg == ".." {
            return Err(VitralError::InvalidArgument(format!(
                "invalid WebDAV path traversal segment in '{rel_path}'"
            )));
        }
    }
    if !allow_empty && trimmed.trim_end_matches('/').is_empty() {
        return Err(VitralError::InvalidArgument(
            "WebDAV file path cannot be empty".into(),
        ));
    }
    Ok(trimmed.to_string())
}

pub fn validate_clean_root(root: Option<&str>) -> Result<String, VitralError> {
    if let Some(raw) = root {
        if raw.contains('\0') || raw.contains('\\') {
            return Err(VitralError::InvalidArgument(
                "invalid koofr_root: contains NUL or backslash".into(),
            ));
        }
        for seg in raw.split('/') {
            let s = seg.trim();
            if s == "." || s == ".." {
                return Err(VitralError::InvalidArgument(format!(
                    "invalid koofr_root path traversal segment in '{raw}'"
                )));
            }
        }
    }
    Ok(normalize_clean_root(root))
}

pub fn normalize_clean_root(root: Option<&str>) -> String {
    root.map(|r| {
        let trimmed = r.trim().trim_matches('/');
        let stripped = if let Some(rest) = trimmed.strip_prefix("Koofr/") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("koofr/") {
            rest
        } else if trimmed.eq_ignore_ascii_case("Koofr") {
            ""
        } else {
            trimmed
        };
        stripped.trim_matches('/')
    })
    .filter(|r| !r.is_empty())
    .unwrap_or("Vitral")
    .to_string()
}

fn dav_error_summary(message: &str) -> String {
    // OpenDAL includes status and headers before its response body. Do not expose either.
    let head = message.split('<').next().unwrap_or_default();
    let status = head.split_once("status").and_then(|(_, status)| {
        status
            .trim_start_matches([':', ' ', '='])
            .split(|c: char| !c.is_ascii_digit())
            .next()
    });
    match status {
        Some("401") => "WebDAV 认证失败 (401)：请检查账户、密码或端点路径".into(),
        Some("403") => "WebDAV 权限被拒 (403)".into(),
        Some("404") => "WebDAV 路径不存在 (404)".into(),
        _ => {
            let end = [
                "headers",
                "Headers",
                "response",
                "Response",
                "report-to",
                "body",
            ]
            .iter()
            .filter_map(|marker| head.find(marker))
            .min()
            .unwrap_or(head.len());
            let summary = &head[..end];
            let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
            format!(
                "WebDAV 操作失败：{}",
                summary.chars().take(160).collect::<String>()
            )
        }
    }
}

fn dav_http_client(follow_redirects: bool) -> Result<reqwest::Client, VitralError> {
    reqwest::Client::builder()
        .user_agent(concat!("Vitral/", env!("CARGO_PKG_VERSION")))
        .tls_backend_preconfigured(rustls_client_config().clone())
        .redirect(if follow_redirects {
            reqwest::redirect::Policy::default()
        } else {
            reqwest::redirect::Policy::none()
        })
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| VitralError::Io(format!("reqwest client build failed: {e}")))
}

pub struct KoofrDavClient {
    op: blocking::Operator,
    cfg: DavConfig,
    guard: OfflineGuard,
    clean_root: String,
    sync_key: Option<String>,
}

impl KoofrDavClient {
    fn build_operator(cfg: &DavConfig, disable_create_dir: bool) -> Result<(blocking::Operator, String), VitralError> {
        let trimmed_ep = cfg.endpoint.trim().trim_end_matches('/');
        if trimmed_ep.is_empty() {
            return Err(VitralError::InvalidArgument(
                "koofr_endpoint cannot be empty".into(),
            ));
        }
        if !trimmed_ep.starts_with("http://") && !trimmed_ep.starts_with("https://") {
            return Err(VitralError::InvalidArgument(format!(
                "unsupported WebDAV endpoint URL scheme: {trimmed_ep}"
            )));
        }

        let clean_root = validate_clean_root(cfg.root.as_deref())?;
        let opendal_root = format!("/{clean_root}/");

        let client = dav_http_client(true)?;

        let mut builder = opendal::services::Webdav::default()
            .endpoint(trimmed_ep)
            .root(&opendal_root)
            .disable_create_dir(disable_create_dir);
        if let Some(user) = cfg
            .username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            builder = builder.username(user);
        }
        if let Some(password) = cfg
            .password
            .as_deref()
            .map(|s| s.trim_matches(['\r', '\n']))
            .filter(|s| !s.is_empty())
        {
            builder = builder.password(password);
        }

        let transport = HttpTransporter::new(ReqwestTransport::new(client));
        let base_op = Operator::new(builder)
            .map_err(|e| VitralError::Io(format!("OpenDAL WebDAV builder failed: {e}")))?
            .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(15)));
        let ctx = base_op.base_context().with_http_transport(transport);
        let async_op = base_op.with_context(ctx);

        let rt = dav_runtime()?;
        let _rt_guard = rt.enter();
        let blocking_op = blocking::Operator::new(async_op)
            .map_err(|e| VitralError::Io(format!("OpenDAL blocking operator init failed: {e}")))?;
        Ok((blocking_op, clean_root))
    }

    pub fn connect(guard: &OfflineGuard, cfg: &DavConfig) -> Result<Self, VitralError> {
        guard.require_online("koofr_webdav")?;
        let (op, clean_root) =
            run_on_blocking_dav_thread(|| Self::build_operator(cfg, false))?;
        Ok(Self {
            op,
            cfg: cfg.clone(),
            guard: guard.clone(),
            clean_root,
            sync_key: cfg.sync_key.clone(),
        })
    }

    pub fn clean_root(&self) -> &str {
        &self.clean_root
    }

    fn ensure_collections(&self, rel_parent: &str) -> Result<(), VitralError> {
        self.guard.require_online("dav_mkcol")?;
        let mut url = reqwest::Url::parse(self.cfg.endpoint.trim())
            .map_err(|e| VitralError::InvalidArgument(e.to_string()))?;
        let client = dav_http_client(false)?;
        let method = reqwest::Method::from_bytes(b"MKCOL")
            .map_err(|e| VitralError::Internal(e.to_string()))?;
        run_on_blocking_dav_thread(|| {
            dav_runtime()?.block_on(async {
                for segment in self
                    .clean_root
                    .split('/')
                    .chain(rel_parent.split('/'))
                    .filter(|s| !s.is_empty())
                {
                    url.path_segments_mut()
                        .map_err(|_| {
                            VitralError::InvalidArgument("invalid WebDAV endpoint".into())
                        })?
                        .pop_if_empty()
                        .push(segment)
                        .push("");
                    self.guard.require_online("dav_mkcol")?;
                    let mut request = client.request(method.clone(), url.clone());
                    if let Some(user) = self
                        .cfg
                        .username
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        request = request.basic_auth(
                            user,
                            self.cfg
                                .password
                                .as_deref()
                                .map(|s| s.trim_matches(['\r', '\n']))
                                .filter(|s| !s.is_empty()),
                        );
                    }
                    let response = request
                        .send()
                        .await
                        .map_err(|e| VitralError::Io(e.without_url().to_string()))?;
                    if !response.status().is_success()
                        && !matches!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED
                            | reqwest::StatusCode::MOVED_PERMANENTLY | reqwest::StatusCode::FOUND)
                    {
                        return Err(VitralError::Io(dav_error_summary(&format!(
                            "HTTP status {}",
                            response.status()
                        ))));
                    }
                }
                Ok(())
            })
        })
    }

    pub fn write_bytes(&self, rel_path: &str, data: &[u8]) -> Result<(), VitralError> {
        self.guard.require_online("dav_write")?;
        let rel = validate_rel_path(rel_path, false)?;
        if rel.ends_with("live_slot.enc") {
            return Err(VitralError::InvalidArgument(
                "live_slot.enc is forbidden by PROPOSAL v1.2".into(),
            ));
        }
        run_on_blocking_dav_thread(|| match self.op.write(&rel, data.to_vec()) {
            Ok(_) => Ok(()),
            Err(first_err) => {
                let msg = first_err.to_string();
                if msg.contains("deserialize xml") {
                    self.guard.require_online("dav_fallback_reconnect")?;
                    let (direct_op, _) = Self::build_operator(&self.cfg, true)?;
                    return direct_op
                        .write(&rel, data.to_vec())
                        .map(|_| ())
                        .map_err(|e| VitralError::Io(dav_error_summary(&e.to_string())));
                }
                let parent_dir = rel.rsplit_once('/').map_or("", |(parent, _)| parent);
                self.ensure_collections(parent_dir)?;
                self.op
                    .write(&rel, data.to_vec())
                    .map(|_| ())
                    .map_err(|e| VitralError::Io(dav_error_summary(&e.to_string())))
            }
        })
    }

    pub fn read_bytes_opt(&self, rel_path: &str) -> Result<Option<Vec<u8>>, VitralError> {
        self.guard.require_online("dav_read")?;
        let rel = validate_rel_path(rel_path, false)?;
        run_on_blocking_dav_thread(|| match self.op.read(&rel) {
            Ok(buf) => Ok(Some(buf.to_vec())),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(VitralError::Io(dav_error_summary(&e.to_string()))),
        })
    }

    pub fn read_bytes(&self, rel_path: &str) -> Result<Vec<u8>, VitralError> {
        self.guard.require_online("dav_read")?;
        let rel = validate_rel_path(rel_path, false)?;
        run_on_blocking_dav_thread(|| {
            self.op
                .read(&rel)
                .map(|b| b.to_vec())
                .map_err(|e| VitralError::Io(dav_error_summary(&e.to_string())))
        })
    }

    pub fn list_dir(&self, rel_dir: &str) -> Result<Vec<opendal::Entry>, VitralError> {
        self.guard.require_online("dav_list")?;
        let clean = validate_rel_path(rel_dir, true)?;
        let dir = if clean.ends_with('/') {
            clean
        } else {
            format!("{clean}/")
        };
        run_on_blocking_dav_thread(|| match self.op.list(&dir) {
            Ok(entries) => Ok(entries),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(VitralError::Io(dav_error_summary(&e.to_string()))),
        })
    }

    pub fn directory_bytes(&self, directory: &str) -> Result<u64, VitralError> {
        let mut folders = vec![directory.to_owned()];
        let mut visited = BTreeSet::new();
        let mut total = 0u64;
        while let Some(folder) = folders.pop() {
            if !visited.insert(folder.clone()) { continue; }
            for entry in self.list_dir(&folder)? {
                let path = entry.path().trim_start_matches('/');
                if path == folder || !path.starts_with(directory) { continue; }
                if entry.metadata().is_dir() {
                    folders.push(path.to_owned());
                } else {
                    self.guard.require_online("storage")?;
                    let len = run_on_blocking_dav_thread(|| {
                        self.op
                            .stat(path)
                            .map(|m| m.content_length())
                            .map_err(|e| VitralError::Io(dav_error_summary(&e.to_string())))
                    })?;
                    total = total.saturating_add(len);
                }
            }
        }
        Ok(total)
    }

    pub fn stat_mtime(&self, rel_path: &str) -> Option<SystemTime> {
        self.guard.require_online("dav_stat").ok()?;
        let rel = validate_rel_path(rel_path, false).ok()?;
        run_on_blocking_dav_thread(|| {
            Ok(self
                .op
                .stat(&rel)
                .ok()
                .and_then(|m| m.last_modified().map(SystemTime::from)))
        })
        .ok()
        .flatten()
    }

    pub fn delete_file(&self, rel_path: &str) -> Result<(), VitralError> {
        self.guard.require_online("dav_delete")?;
        let rel = validate_rel_path(rel_path, false)?;
        run_on_blocking_dav_thread(|| match self.op.delete(&rel) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(VitralError::Io(dav_error_summary(&e.to_string()))),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayMeta {
    #[serde(default)]
    pub transfer_id: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub created_at_utc: String,
    #[serde(default)]
    pub target_device_id: String,
    #[serde(default)]
    pub source_device_id: String,
    #[serde(default)]
    pub is_file: bool,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayPayloadEnvelope {
    vitral_relay: u32,
    is_file: bool,
    file_name: String,
    #[serde(default)]
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload_b64: Option<String>,
}

impl RelayPayloadEnvelope {
    fn decode_payload_bytes(&self) -> Result<Vec<u8>, VitralError> {
        if let Some(ref b64) = self.payload_b64 {
            base64_decode(b64)
        } else {
            Ok(self.content.as_bytes().to_vec())
        }
    }
}

pub fn upload_lexicon_snapshot(
    guard: &OfflineGuard,
    cfg: &DavConfig,
    snapshot: &LexiconSnapshotPayload,
) -> Result<(), VitralError> {
    guard.require_online("koofr_upload_lexicon_snapshot")?;
    let client = KoofrDavClient::connect(guard, cfg)?;
    let json = serde_json::to_vec(snapshot)
        .map_err(|e| VitralError::Internal(format!("snapshot serialize failed: {e}")))?;
    let compressed = zstd_compress(&json)?;
    let encrypted = encrypt_envelope(&compressed, cfg.sync_key.as_deref())?;
    client.write_bytes("lexicon/snapshot.zst.enc", &encrypted)
}

pub fn upload_retention_crdt_delta(
    guard: &OfflineGuard,
    cfg: &DavConfig,
    delta: &RetentionCrdtDelta,
) -> Result<(), VitralError> {
    guard.require_online("koofr_upload_retention_crdt")?;
    let client = KoofrDavClient::connect(guard, cfg)?;
    let mut cbor = Vec::new();
    ciborium::into_writer(delta, &mut cbor)
        .map_err(|e| VitralError::Internal(format!("CRDT CBOR encode failed: {e}")))?;
    let encrypted = encrypt_envelope(&cbor, cfg.sync_key.as_deref())?;
    let safe_dev: String = delta
        .device_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = format!("lexicon/crdt/{safe_dev}_{}.cbor.enc", delta.seq);
    client.write_bytes(&path, &encrypted)
}

pub fn upload_profile_config(
    guard: &OfflineGuard,
    cfg: &DavConfig,
    profile: &ProfileBackup,
) -> Result<(), VitralError> {
    guard.require_online("koofr_upload_profile_config")?;
    let client = KoofrDavClient::connect(guard, cfg)?;
    let json = serde_json::to_vec(profile)
        .map_err(|e| VitralError::Internal(format!("profile serialize failed: {e}")))?;
    let encrypted = encrypt_envelope(&json, cfg.sync_key.as_deref())?;
    client.write_bytes("profile/config.json.enc", &encrypted)
}

fn read_clipboard_ring(client: &KoofrDavClient, key: Option<&str>) -> Result<Option<Vec<ClipboardEntry>>, VitralError> {
    client.read_bytes_opt("clipboard/history_ring.jsonl.enc")?.map(|enc| {
        decode_clipboard_ring_jsonl(&decrypt_envelope(&enc,key)?)
    }).transpose()
}

pub fn download_clipboard_ring(guard:&OfflineGuard,cfg:&DavConfig)->Result<Option<Vec<ClipboardEntry>>,VitralError>{
    let client=KoofrDavClient::connect(guard,cfg)?;
    read_clipboard_ring(&client,cfg.sync_key.as_deref())
}

/// OpenDAL's WebDAV writer doesn't expose If-Match. Use the same HTTP/TLS
/// configuration for this shared, versioned object and retry on a competing PUT.
pub(crate) fn merge_clipboard_ring(
    guard: &OfflineGuard,
    cfg: &DavConfig,
    changes: &crate::clip::RingChanges,
    local: &[ClipboardEntry],
    capacity: usize,
) -> Result<Vec<ClipboardEntry>, VitralError> {
    let dav = KoofrDavClient::connect(guard, cfg)?;
    let mut url = reqwest::Url::parse(&cfg.endpoint).map_err(|e| VitralError::InvalidArgument(e.to_string()))?;
    {
        let mut path = url.path_segments_mut().map_err(|_| VitralError::InvalidArgument("invalid WebDAV endpoint".into()))?;
        path.pop_if_empty().extend(dav.clean_root.split('/')).push("clipboard").push("history_ring.jsonl.enc");
    }
    let client = dav_http_client(true)?;
    let request = |method| {
        let req = client.request(method, url.clone());
        if let Some(user) = cfg.username.as_ref() { req.basic_auth(user, cfg.password.as_ref()) } else { req }
    };
    let io = |e: reqwest::Error| VitralError::Io(e.without_url().to_string());
    run_on_blocking_dav_thread(|| {
        for _ in 0..4 {
            let conditional = dav_runtime()?.block_on(async {
                guard.require_online("read clipboard revision")?;
                let reply = request(reqwest::Method::GET).send().await.map_err(io)?;
                let (cloud, tag, missing) = if reply.status() == reqwest::StatusCode::NOT_FOUND {
                    (local.to_vec(), None, true)
                } else {
                    let reply = reply.error_for_status().map_err(io)?;
                    let tag = reply.headers().get(reqwest::header::ETAG).cloned();
                    let enc = reply.bytes().await.map_err(io)?;
                    (decode_clipboard_ring_jsonl(&decrypt_envelope(&enc, cfg.sync_key.as_deref())?)?, tag, false)
                };
                let merged = changes.merge(cloud, local, capacity);
                let data = encrypt_envelope(&encode_clipboard_ring_jsonl(&merged)?, cfg.sync_key.as_deref())?;
                let mut put = request(reqwest::Method::PUT);
                if missing { put = put.header(reqwest::header::IF_NONE_MATCH, "*"); }
                else if let Some(tag) = tag { put = put.header(reqwest::header::IF_MATCH, tag); }
                guard.require_online("write clipboard revision")?;
                let result = put.body(data).send().await.map_err(io)?;
                match result.status() {
                    reqwest::StatusCode::PRECONDITION_FAILED => Ok::<_, VitralError>(None),
                    reqwest::StatusCode::CONFLICT | reqwest::StatusCode::NOT_FOUND if missing => Ok(Some((false, merged))),
                    _ => { result.error_for_status().map_err(io)?; Ok(Some((true, merged))) }
                }
            })?;
            match conditional {
                Some((true, entries)) => return Ok(entries),
                Some((false, _)) => {
                    dav.ensure_collections("clipboard")?;
                }
                None => {}
            }
        }
        Err(VitralError::Io("clipboard changed during synchronization".into()))
    })
}

/// The latest copy survives history trimming and device-local suggestion dismissal.
pub(crate) fn sync_current_copy(
    guard: &OfflineGuard, cfg: &DavConfig, local: Option<&ClipboardEntry>,
) -> Result<Option<ClipboardEntry>, VitralError> {
    let dav = KoofrDavClient::connect(guard, cfg)?;
    let mut url = reqwest::Url::parse(&cfg.endpoint).map_err(|e| VitralError::InvalidArgument(e.to_string()))?;
    url.path_segments_mut().map_err(|_| VitralError::InvalidArgument("invalid WebDAV endpoint".into()))?
        .pop_if_empty().extend(dav.clean_root.split('/')).push("clipboard").push("current.json.enc");
    let client = dav_http_client(true)?;
    let request = |method| {
        let req = client.request(method, url.clone());
        if let Some(user) = cfg.username.as_ref() { req.basic_auth(user, cfg.password.as_ref()) } else { req }
    };
    let io = |e: reqwest::Error| VitralError::Io(e.without_url().to_string());
    run_on_blocking_dav_thread(|| {
        for _ in 0..4 {
            let outcome = dav_runtime()?.block_on(async {
                guard.require_online("synchronize current clipboard")?;
                let reply = request(reqwest::Method::GET).send().await.map_err(io)?;
                let missing = reply.status() == reqwest::StatusCode::NOT_FOUND;
                let (cloud, tag) = if missing { (None, None) } else {
                    let reply = reply.error_for_status().map_err(io)?;
                    let tag = reply.headers().get(reqwest::header::ETAG).cloned();
                    let data = reply.bytes().await.map_err(io)?;
                    (Some(serde_json::from_slice::<ClipboardEntry>(&decrypt_envelope(&data, cfg.sync_key.as_deref())?)
                        .map_err(|e| VitralError::Parse(e.to_string()))?), tag)
                };
                let Some(local) = local else { return Ok((true, cloud)); };
                if cloud.as_ref().is_some_and(|e| crate::clip::copy_order(e) >= crate::clip::copy_order(local)) {
                    return Ok((true, cloud));
                }
                let mut put = request(reqwest::Method::PUT);
                if missing { put = put.header(reqwest::header::IF_NONE_MATCH, "*"); }
                else { put = put.header(reqwest::header::IF_MATCH, tag.ok_or_else(|| VitralError::Io("WebDAV current clipboard response is missing ETag".into()))?); }
                let data = serde_json::to_vec(local).map_err(|e| VitralError::Parse(e.to_string()))?;
                guard.require_online("write current clipboard")?;
                let response = put.body(encrypt_envelope(&data, cfg.sync_key.as_deref())?).send().await.map_err(io)?;
                match response.status() {
                    reqwest::StatusCode::PRECONDITION_FAILED => Ok((false, None)),
                    reqwest::StatusCode::CONFLICT | reqwest::StatusCode::NOT_FOUND if missing => Ok((false, Some(local.clone()))),
                    _ => { response.error_for_status().map_err(io)?; Ok::<_, VitralError>((true, Some(local.clone()))) }
                }
            })?;
            if outcome.0 { return Ok(outcome.1); }
            if outcome.1.is_some() { dav.ensure_collections("clipboard")?; }
        }
        Err(VitralError::Io("current clipboard changed during synchronization".into()))
    })
}

pub fn upload_clipboard_ring(
    guard: &OfflineGuard,
    cfg: &DavConfig,
    entries: &[ClipboardEntry],
) -> Result<(), VitralError> {
    guard.require_online("koofr_upload_clipboard_ring")?;
    let client = KoofrDavClient::connect(guard, cfg)?;
    let jsonl = encode_clipboard_ring_jsonl(entries)?;
    let encrypted = encrypt_envelope(&jsonl, cfg.sync_key.as_deref())?;
    client.write_bytes("clipboard/history_ring.jsonl.enc", &encrypted)
}

#[allow(clippy::too_many_arguments)]
pub fn upload_koofr_relay(
    guard: &OfflineGuard,
    cfg: &DavConfig,
    source_device_id: &str,
    target_device_id: &str,
    transfer_id: &str,
    file_name: &str,
    is_file: bool,
    payload_bytes: &[u8],
) -> Result<RelaySendResult, VitralError> {
    guard.require_online("koofr_relay_send")?;
    validate_safe_id("transfer_id", transfer_id)?;
    let client = KoofrDavClient::connect(guard, cfg)?;
    let utf8_text = std::str::from_utf8(payload_bytes).ok();
    let content = utf8_text.map(str::to_string).unwrap_or_default();
    let payload_b64 = if is_file || utf8_text.is_none() {
        Some(base64_encode(payload_bytes))
    } else {
        None
    };
    let env = RelayPayloadEnvelope {
        vitral_relay: 1,
        is_file,
        file_name: file_name.to_string(),
        content,
        payload_b64,
    };
    let plaintext = serde_json::to_vec(&env)
        .map_err(|e| VitralError::Internal(format!("relay envelope encode failed: {e}")))?;
    let enc_bytes = encrypt_envelope(&plaintext, cfg.sync_key.as_deref())?;
    let enc_sha256 = sha256_hex(&enc_bytes);

    let meta = RelayMeta {
        transfer_id: transfer_id.to_string(),
        sha256: enc_sha256.clone(),
        created_at_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        target_device_id: target_device_id.to_string(),
        source_device_id: source_device_id.to_string(),
        is_file,
        file_name: file_name.to_string(),
        size: enc_bytes.len(),
    };
    let meta_bytes = serde_json::to_vec(&meta)
        .map_err(|e| VitralError::Internal(format!("relay meta encode failed: {e}")))?;

    client.write_bytes(&format!("relay/{transfer_id}.enc"), &enc_bytes)?;
    client.write_bytes(&format!("relay/{transfer_id}.meta.json"), &meta_bytes)?;

    Ok(RelaySendResult {
        transfer_id: transfer_id.to_string(),
        device_id: target_device_id.to_string(),
        transport: "koofr_webdav_relay".to_string(),
        sha256: enc_sha256,
        status: "sent".to_string(),
    })
}

pub fn poll_koofr_relay_with_config(
    guard: &OfflineGuard,
    cfg: Option<&DavConfig>,
    local_device_id: &str,
) -> Result<RelayPollResult, VitralError> {
    guard.require_online("koofr_relay_poll")?;
    let Some(cfg) = cfg else {
        return Ok(RelayPollResult {
            received: Vec::new(),
            deleted_count: 0,
            received_items: Vec::new(),
        });
    };

    let client = KoofrDavClient::connect(guard, cfg)?;
    let entries = client.list_dir("relay/")?;
    let mut meta_paths: Vec<String> = entries
        .into_iter()
        .map(|e| e.path().trim_start_matches('/').to_string())
        .filter(|p| p.ends_with(".meta.json"))
        .collect();
    meta_paths.sort();

    let mut received = Vec::new();
    let mut received_items = Vec::new();
    let mut deleted_count = 0usize;

    for meta_path in meta_paths {
        let Some(meta_bytes) = client.read_bytes_opt(&meta_path)? else {
            continue;
        };
        let meta: RelayMeta = serde_json::from_slice(&meta_bytes)
            .map_err(|e| VitralError::Integrity(format!("invalid relay meta {meta_path}: {e}")))?;
        if !meta.target_device_id.is_empty()
            && !local_device_id.is_empty()
            && meta.target_device_id != local_device_id
        {
            continue;
        }
        let stem = meta_path.strip_suffix(".meta.json").unwrap_or(&meta_path);
        let enc_path = format!("{stem}.enc");
        let Some(enc_bytes) = client.read_bytes_opt(&enc_path)? else {
            continue;
        };

        let expected_sha = meta.sha256.trim().to_ascii_lowercase();
        if expected_sha.is_empty() {
            return Err(VitralError::Integrity(format!(
                "relay meta missing sha256 for {}",
                meta.transfer_id
            )));
        }
        let actual_sha = sha256_hex(&enc_bytes);
        if actual_sha != expected_sha {
            return Err(VitralError::Integrity(format!(
                "relay SHA-256 mismatch for {}: expected {}, actual {}",
                meta.transfer_id, meta.sha256, actual_sha
            )));
        }

        let plaintext = decrypt_envelope(&enc_bytes, client.sync_key.as_deref())?;
        let (text, item) = if let Ok(env) = serde_json::from_slice::<RelayPayloadEnvelope>(&plaintext) {
            let raw_bytes = env.decode_payload_bytes()?;
            let text_val = if !env.content.is_empty() || raw_bytes.is_empty() {
                env.content.clone()
            } else if let Ok(s) = std::str::from_utf8(&raw_bytes) {
                s.to_string()
            } else {
                String::from_utf8_lossy(&raw_bytes).into_owned()
            };
            let fname = if env.file_name.is_empty() {
                None
            } else {
                Some(env.file_name)
            };
            let item = RelayReceivedItem {
                transfer_id: meta.transfer_id.clone(),
                raw_bytes,
                is_file: env.is_file,
                file_name: fname,
            };
            (text_val, item)
        } else {
            let text_val = match std::str::from_utf8(&plaintext) {
                Ok(s) => s.to_string(),
                Err(e) if !meta.is_file => {
                    return Err(VitralError::Integrity(format!("relay UTF-8 decode failed: {e}")));
                }
                Err(_) => String::from_utf8_lossy(&plaintext).into_owned(),
            };
            let fname = if meta.file_name.is_empty() {
                None
            } else {
                Some(meta.file_name.clone())
            };
            let item = RelayReceivedItem {
                transfer_id: meta.transfer_id.clone(),
                raw_bytes: plaintext,
                is_file: meta.is_file,
                file_name: fname,
            };
            (text_val, item)
        };

        client.delete_file(&enc_path)?;
        client.delete_file(&meta_path)?;
        deleted_count += 2;
        received.push(text);
        received_items.push(item);
    }

    Ok(RelayPollResult {
        received,
        deleted_count,
        received_items,
    })
}

pub fn cleanup_koofr_orphans_with_config(
    guard: &OfflineGuard,
    cfg: Option<&DavConfig>,
) -> Result<CleanupResult, VitralError> {
    guard.require_online("koofr_relay_cleanup_orphans")?;
    let Some(cfg) = cfg else {
        return Ok(CleanupResult { purged_count: 0 });
    };
    let client = KoofrDavClient::connect(guard, cfg)?;
    let entries = client.list_dir("relay/")?;
    let now = SystemTime::now();
    let max_age = Duration::from_secs(86_400);

    let mut all_files: HashMap<String, Vec<String>> = HashMap::new();
    let mut expired_stems: BTreeSet<String> = BTreeSet::new();
    let mut standalone_expired: Vec<String> = Vec::new();

    for entry in entries {
        let rel = entry.path().trim_start_matches('/').to_string();
        if rel.is_empty() || rel.ends_with('/') || rel == "relay" {
            continue;
        }
        let stem = rel
            .strip_suffix(".meta.json")
            .or_else(|| rel.strip_suffix(".enc"))
            .map(str::to_string);
        if let Some(ref s) = stem {
            all_files.entry(s.clone()).or_default().push(rel.clone());
        }

        let mut mtime = entry
            .metadata()
            .last_modified()
            .map(SystemTime::from)
            .or_else(|| client.stat_mtime(&rel));

        if mtime.is_none() && rel.ends_with(".meta.json") {
            if let Ok(Some(meta_bytes)) = client.read_bytes_opt(&rel) {
                if let Ok(meta) = serde_json::from_slice::<RelayMeta>(&meta_bytes) {
                    if let Ok(dt) = DateTime::parse_from_rfc3339(&meta.created_at_utc) {
                        mtime = Some(SystemTime::from(dt));
                    }
                }
            }
        }

        let is_expired = mtime
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);

        if is_expired {
            if let Some(s) = stem {
                expired_stems.insert(s);
            } else {
                standalone_expired.push(rel);
            }
        }
    }

    let mut purged_count = 0usize;
    let mut deleted_paths: BTreeSet<String> = BTreeSet::new();
    for stem in expired_stems {
        if let Some(files) = all_files.get(&stem) {
            for f in files {
                if deleted_paths.insert(f.clone()) {
                    client.delete_file(f)?;
                    purged_count += 1;
                }
            }
        }
    }
    for path in standalone_expired {
        if deleted_paths.insert(path.clone()) {
            client.delete_file(&path)?;
            purged_count += 1;
        }
    }

    Ok(CleanupResult { purged_count })
}

#[derive(Debug)]
pub struct FetchedBackup {
    pub profile: Option<ProfileBackup>,
    pub snapshot: Option<LexiconSnapshotPayload>,
    pub crdt_deltas: Vec<RetentionCrdtDelta>,
    pub import_artifacts: Vec<LexiconImportArtifact>,
    pub clipboard_ring: Option<Vec<ClipboardEntry>>,
    pub snapshots_applied: usize,
    pub cloud_root: String,
}

pub fn fetch_backup_from_koofr(
    guard: &OfflineGuard,
    cfg: &DavConfig,
) -> Result<FetchedBackup, VitralError> {
    guard.require_online("restore_from_koofr")?;
    let client = KoofrDavClient::connect(guard, cfg)?;
    let sync_key = cfg.sync_key.as_deref();
    let mut snapshots_applied = 0usize;

    let profile = if let Some(enc) = client.read_bytes_opt("profile/config.json.enc")? {
        let pt = decrypt_envelope(&enc, sync_key)?;
        let parsed: ProfileBackup = serde_json::from_slice(&pt)
            .map_err(|e| VitralError::Integrity(format!("corrupt profile backup: {e}")))?;
        snapshots_applied += 1;
        Some(parsed)
    } else {
        None
    };

    let snapshot = if let Some(enc) = client.read_bytes_opt("lexicon/snapshot.zst.enc")? {
        let pt = decrypt_envelope(&enc, sync_key)?;
        let decompressed = zstd_decompress(&pt)?;
        let parsed: LexiconSnapshotPayload = serde_json::from_slice(&decompressed)
            .map_err(|e| VitralError::Integrity(format!("corrupt lexicon snapshot: {e}")))?;
        snapshots_applied += 1;
        Some(parsed)
    } else {
        None
    };

    let mut crdt_paths: Vec<String> = client
        .list_dir("lexicon/crdt/")?
        .into_iter()
        .map(|e| e.path().trim_start_matches('/').to_string())
        .filter(|p| p.ends_with(".cbor.enc"))
        .collect();
    crdt_paths.sort();
    let mut crdt_deltas = Vec::with_capacity(crdt_paths.len());
    for p in crdt_paths {
        let Some(enc) = client.read_bytes_opt(&p)? else {
            continue;
        };
        let pt = decrypt_envelope(&enc, sync_key)?;
        let delta: RetentionCrdtDelta = ciborium::from_reader(pt.as_slice())
            .map_err(|e| VitralError::Integrity(format!("corrupt CRDT delta {p}: {e}")))?;
        crdt_deltas.push(delta);
        snapshots_applied += 1;
    }
    crdt_deltas.sort_by_key(|d| (d.timestamp_ms, d.seq));

    let mut import_paths: Vec<String> = client
        .list_dir("lexicon/imports/")?
        .into_iter()
        .map(|e| e.path().trim_start_matches('/').to_string())
        .filter(|p| p.ends_with(".cbor"))
        .collect();
    import_paths.sort();
    let mut import_artifacts = Vec::with_capacity(import_paths.len());
    for p in import_paths {
        let Some(cbor) = client.read_bytes_opt(&p)? else {
            continue;
        };
        let art: LexiconImportArtifact = ciborium::from_reader(cbor.as_slice())
            .map_err(|e| VitralError::Integrity(format!("corrupt lexicon import {p}: {e}")))?;
        import_artifacts.push(art);
        snapshots_applied += 1;
    }

    let clipboard_ring = read_clipboard_ring(&client,sync_key)?;
    snapshots_applied += usize::from(clipboard_ring.is_some());

    if snapshots_applied == 0 {
        return Err(VitralError::NotFound(
            "no Vitral backup snapshots found on Koofr WebDAV".into(),
        ));
    }

    Ok(FetchedBackup {
        profile,
        snapshot,
        crdt_deltas,
        import_artifacts,
        clipboard_ring,
        snapshots_applied,
        cloud_root: format!("/Koofr/{}", client.clean_root()),
    })
}

pub fn poll_koofr_relay(guard: &OfflineGuard) -> Result<RelayPollResult, VitralError> {
    poll_koofr_relay_with_config(guard, None, "")
}

pub fn cleanup_koofr_orphans(guard: &OfflineGuard) -> Result<CleanupResult, VitralError> {
    cleanup_koofr_orphans_with_config(guard, None)
}

pub fn restore_from_koofr(
    guard: &OfflineGuard,
    webdav_cfg_json: &str,
) -> Result<RestoreResult, VitralError> {
    guard.require_online("restore_from_koofr")?;
    if webdav_cfg_json.trim().is_empty() {
        return Err(VitralError::InvalidArgument(
            "webdav_cfg_json cannot be empty".into(),
        ));
    }
    let val: serde_json::Value = serde_json::from_str(webdav_cfg_json)
        .map_err(|e| VitralError::Parse(format!("invalid webdav_cfg_json: {e}")))?;
    let has_ep = val
        .get("koofr_endpoint")
        .or_else(|| val.get("webdav_url"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    if has_ep {
        let cfg = DavConfig::from_json_with_fallback(webdav_cfg_json, None)?;
        let fetched = fetch_backup_from_koofr(guard, &cfg)?;
        Ok(RestoreResult {
            restored: true,
            cloud_root: fetched.cloud_root,
            snapshots_applied: fetched.snapshots_applied,
        })
    } else {
        Ok(RestoreResult {
            restored: true,
            cloud_root: TOKENS.brand.cloud_root.to_string(),
            snapshots_applied: 0,
        })
    }
}

/// Upload a deterministic lexicon import artifact under `/Koofr/Vitral/lexicon/imports/{filename}`
/// via Apache `OpenDAL` (`services-webdav`), strictly guarded by `OfflineGuard::require_online`.
pub fn put_lexicon_import_artifact(
    guard: &OfflineGuard,
    endpoint: &str,
    username: Option<&str>,
    password: Option<&str>,
    root: Option<&str>,
    filename: &str,
    bytes: &[u8],
) -> Result<String, VitralError> {
    guard.require_online("koofr_lexicon_import_put")?;
    validate_safe_id("filename", filename)?;
    let cfg = DavConfig {
        endpoint: endpoint.to_string(),
        username: username.map(str::to_string),
        password: password.map(str::to_string),
        root: root.map(str::to_string),
        sync_key: None,
    };
    let client = KoofrDavClient::connect(guard, &cfg)?;
    let rel_file = format!("lexicon/imports/{filename}");
    client.write_bytes(&rel_file, bytes)?;
    Ok(format!("/Koofr/{}/{rel_file}", client.clean_root()))
}

#[cfg(test)]
mod device_tests {
    use super::*;

    #[test]
    fn credentials_and_error_summaries() {
        let config = EngineConfig {
            koofr_endpoint: Some(" https://example.org/dav/ ".into()),
            koofr_username: Some(" user@example.org \t".into()),
            koofr_password: Some("\r\n password \n".into()),
            ..Default::default()
        };
        let cfg = DavConfig::from_engine_config(&config).unwrap();
        assert_eq!(cfg.endpoint, "https://example.org/dav/");
        assert_eq!(cfg.username.as_deref(), Some("user@example.org"));
        assert_eq!(cfg.password.as_deref(), Some(" password "));
        let parsed = DavConfig::from_json_with_fallback(r#"{"koofr_endpoint":" https://example.org/dav/ ","koofr_username":" user@example.org ","koofr_password":"\r\nsecret\r\n"}"#, None).unwrap();
        assert_eq!(parsed.endpoint, cfg.endpoint);
        assert_eq!(parsed.username, cfg.username);
        assert_eq!(parsed.password.as_deref(), Some("secret"));
        let empty = DavConfig::from_json_with_fallback(
            r#"{"koofr_username":" \t ","koofr_password":"\r\n"}"#,
            Some(&cfg),
        )
        .unwrap();
        assert!(empty.username.is_none());
        assert!(empty.password.is_none());
        let error = dav_error_summary("PermissionDenied status: 401, headers: {report-to: cloudflare}, body: <html>401 Unauthorized</html>");
        assert_eq!(error, "WebDAV 认证失败 (401)：请检查账户、密码或端点路径");
        assert_eq!(
            dav_error_summary("HTTP status 403 Forbidden"),
            "WebDAV 权限被拒 (403)"
        );
        assert_eq!(
            dav_error_summary("HTTP status 404 Not Found"),
            "WebDAV 路径不存在 (404)"
        );
        let error = dav_error_summary(
            "Unexpected path /401 response: Parts { status: 502, headers: report-to: {} } <html>bad gateway</html>",
        );
        assert!(!error.contains("report-to") && !error.contains('<'));
        assert!(!error.contains("认证失败"));
    }
}

#[cfg(test)]
mod http_device_tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn mkcol_accepts_existing_collection_redirects_but_rejects_errors() {
        for status in [201, 301, 302, 405, 401, 403, 500] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let cfg = DavConfig {
                endpoint: format!("http://{}/dav/", server.server_addr()),
                root: Some("Vitral".into()),
                ..Default::default()
            };
            let handle = std::thread::spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), "MKCOL");
                let response = tiny_http::Response::empty(status)
                    .with_header(tiny_http::Header::from_bytes("Location", request.url()).unwrap());
                request.respond(response).unwrap();
            });
            let client = KoofrDavClient::connect(&OfflineGuard::new(false), &cfg).unwrap();
            assert_eq!(
                client.ensure_collections("").is_ok(),
                matches!(status, 201 | 301 | 302 | 405),
                "{status}"
            );
            handle.join().unwrap();
        }
    }

    #[test]
    fn opendal_and_mkcol_trim_auth_and_hide_html() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = socket.read(&mut buf).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buf[..count]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = "<html><title>401 Unauthorized</title></html>";
                write!(socket, "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nContent-Type: text/html\r\nreport-to: {{\"group\":\"cloudflare\"}}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        let cfg = DavConfig {
            endpoint: format!(" http://{address}/dav/ "),
            username: Some(" user ".into()),
            password: Some("\r\npass\r\n".into()),
            root: None,
            sync_key: None,
        };
        let client = KoofrDavClient::connect(&OfflineGuard::new(false), &cfg).unwrap();
        for error in [
            client.read_bytes("file").unwrap_err(),
            client.ensure_collections("").unwrap_err(),
        ] {
            let VitralError::Io(message) = error else {
                panic!("expected I/O error")
            };
            assert_eq!(message, "WebDAV 认证失败 (401)：请检查账户、密码或端点路径");
        }
        for request in server.join().unwrap() {
            let request = request.to_ascii_lowercase();
            assert!(
                request.contains("authorization: basic dxnlcjpwyxnz"),
                "{request}"
            );
            assert!(request.contains("user-agent: vitral/"), "{request}");
        }
    }
}
