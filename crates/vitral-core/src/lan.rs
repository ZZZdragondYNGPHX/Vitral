use crate::clip::ClipboardEntry;
use crate::dav::{upload_koofr_relay, DavConfig};
use crate::error::VitralError;
use crate::offline_guard::OfflineGuard;
use crate::ret::RetentionEngine;
use crate::sync::{
    decrypt_envelope, encrypt_envelope, sha256_hex, sort_devices_mru, DeviceTier, RelayDevice,
    RelaySendResult,
};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const LOCALSEND_MULTICAST_ADDR: &str = "224.0.0.167";
pub const LOCALSEND_DEFAULT_PORT: u16 = 53317;
pub const LOCALSEND_MDNS_TYPE: &str = "_localsend._tcp.local.";
pub const MAX_LOCALSEND_CONTROL_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_LOCALSEND_UPLOAD_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIPBOARD_BODY_BYTES: usize = 256 * 1024;
const MAX_PENDING_CLIPBOARD_ENTRIES: usize = 32;

static RELAY_TX_SEQ: AtomicU64 = AtomicU64::new(1);
static CLIPBOARD_RX_SEQ: AtomicU64 = AtomicU64::new(1);

fn is_safe_query_token(val: &str) -> bool {
    !val.is_empty()
        && val
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn read_bounded_body(req: &mut tiny_http::Request, max_bytes: usize) -> Result<Vec<u8>, u16> {
    if let Some(len) = req.body_length() {
        if len > max_bytes {
            return Err(413);
        }
    }
    let mut body = Vec::new();
    let limit = (max_bytes as u64).saturating_add(1);
    if req.as_reader().take(limit).read_to_end(&mut body).is_err() {
        return Err(400);
    }
    if body.len() > max_bytes {
        return Err(413);
    }
    Ok(body)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSendDeviceInfo {
    pub alias: String,
    pub version: String,
    pub device_model: String,
    pub device_type: String,
    pub fingerprint: String,
    #[serde(default = "default_localsend_port")]
    pub port: u16,
    #[serde(default = "default_localsend_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub download: bool,
    #[serde(default)]
    pub announce: bool,
}

fn default_localsend_port() -> u16 {
    LOCALSEND_DEFAULT_PORT
}
fn default_localsend_protocol() -> String {
    "http".to_string()
}

impl LocalSendDeviceInfo {
    pub fn new(device_id: &str, device_name: &str, port: u16) -> Self {
        Self {
            alias: device_name.to_string(),
            version: "2.1".to_string(),
            device_model: "Vitral".to_string(),
            device_type: "desktop".to_string(),
            fingerprint: device_id.to_string(),
            port,
            protocol: "http".to_string(),
            download: false,
            announce: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReceivedLocalSendTransfer {
    pub session_id: String,
    pub file_id: String,
    pub file_name: String,
    pub size: usize,
    pub sha256_actual: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct PrepareUploadRequest {
    info: serde_json::Value,
    files: HashMap<String, PrepareFileMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareFileMeta {
    #[serde(default)]
    file_name: String,
    size: usize,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareUploadResponse {
    session_id: String,
    files: HashMap<String, String>,
}

struct ServerSession {
    files: HashMap<String, PrepareFileMeta>,
    tokens: HashMap<String, String>,
    uploaded: HashSet<String>,
    cancelled: bool,
}

#[derive(Default)]
struct ClipboardInbox {
    sync_key: Option<String>,
    entries: Vec<ClipboardEntry>,
    revision: u64,
}

impl ClipboardInbox {
    fn has_key(&self) -> bool {
        self.sync_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    }

    fn receive(&mut self, body: &[u8]) -> u16 {
        // The WebDAV format has a built-in fallback key. LAN requires a shared secret.
        let Some(key) = self
            .sync_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
        else {
            return 403;
        };
        let Ok(data) = decrypt_envelope(body, Some(key)) else {
            return 403;
        };
        let Ok(entry) = serde_json::from_slice::<ClipboardEntry>(&data) else {
            return 400;
        };
        if self.entries.len() >= MAX_PENDING_CLIPBOARD_ENTRIES {
            let (index, oldest) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| crate::clip::copy_order(entry))
                .expect("full clipboard inbox");
            if crate::clip::copy_order(&entry) <= crate::clip::copy_order(oldest) {
                return 204;
            }
            self.entries.swap_remove(index);
        }
        self.entries.push(entry);
        self.revision = CLIPBOARD_RX_SEQ.fetch_add(1, Ordering::Relaxed);
        204
    }
}

pub struct LocalSendReceiverHandle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    received: Arc<Mutex<Vec<ReceivedLocalSendTransfer>>>,
    clipboard: Arc<Mutex<ClipboardInbox>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl LocalSendReceiverHandle {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn received(&self) -> Vec<ReceivedLocalSendTransfer> {
        self.received.lock().clone()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for LocalSendReceiverHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct DiscoverySession {
    receiver: LocalSendReceiverHandle,
    mdns: ServiceDaemon,
}

impl DiscoverySession {
    pub fn start(
        guard: &OfflineGuard,
        id: &str,
        name: &str,
        sync_key: Option<String>,
    ) -> Result<Self, VitralError> {
        let mut info = LocalSendDeviceInfo::new(id, name, 0);
        let receiver = LocalSendTransport::start_receiver(guard, "0.0.0.0:0", info.clone())?;
        receiver.clipboard.lock().sync_key = sync_key;
        info.port = receiver.port();
        let mdns = LocalSendTransport::register_mdns(guard, &info)?;
        Ok(Self { receiver, mdns })
    }

    pub fn take_received(&self) -> Vec<ReceivedLocalSendTransfer> {
        std::mem::take(&mut *self.receiver.received.lock())
    }

    pub fn take_received_clipboard(&self) -> Vec<ClipboardEntry> {
        std::mem::take(&mut self.receiver.clipboard.lock().entries)
    }

    pub(crate) fn has_received_clipboard(&self) -> bool {
        !self.receiver.clipboard.lock().entries.is_empty()
    }

    pub(crate) fn clipboard_revision(&self) -> u64 {
        self.receiver.clipboard.try_lock().map_or(0, |inbox| inbox.revision)
    }

    pub(crate) fn set_clipboard_key(&self, sync_key: Option<String>) {
        let mut inbox = self.receiver.clipboard.lock();
        if inbox.sync_key != sync_key {
            inbox.sync_key = sync_key;
            inbox.entries.clear();
        }
    }
}

impl Drop for DiscoverySession {
    fn drop(&mut self) {
        let _ = self.mdns.shutdown();
    }
}

pub struct LocalSendTransport;

impl LocalSendTransport {
    pub fn start_receiver(
        guard: &OfflineGuard,
        bind_addr: &str,
        mut info: LocalSendDeviceInfo,
    ) -> Result<LocalSendReceiverHandle, VitralError> {
        guard.require_online("localsend_start_receiver")?;
        let server = tiny_http::Server::http(bind_addr)
            .map_err(|e| VitralError::Io(format!("LocalSend bind {bind_addr} failed: {e}")))?;
        let addr = match server.server_addr() {
            tiny_http::ListenAddr::IP(sa) => sa,
            #[cfg(unix)]
            _ => {
                return Err(VitralError::Io(
                    "LocalSend server requires IP listen address".into(),
                ))
            }
        };
        info.port = addr.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let received = Arc::new(Mutex::new(Vec::<ReceivedLocalSendTransfer>::new()));
        let rec_bg = Arc::clone(&received);
        let clipboard = Arc::new(Mutex::new(ClipboardInbox::default()));
        let clipboard_bg = Arc::clone(&clipboard);
        let guard = guard.clone();
        let sessions = Arc::new(Mutex::new(HashMap::<String, ServerSession>::new()));
        let sess_seq = Arc::new(AtomicU64::new(1));

        let join = std::thread::spawn(move || {
            while !stop_bg.load(Ordering::SeqCst) {
                let mut req = match server.recv_timeout(Duration::from_millis(50)) {
                    Ok(Some(r)) => r,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let url = req.url().to_string();
                let method = req.method().as_str().to_ascii_uppercase();
                let (path, query_str) = url.split_once('?').unwrap_or((&url, ""));
                let query = parse_query_params(query_str);

                let prefix = "/api/localsend/v2/";
                if !path.starts_with(prefix) {
                    let _ = req.respond(close_text_response(404, "Not Found"));
                    continue;
                }
                let endpoint = path[prefix.len()..].trim_matches('/');

                match (method.as_str(), endpoint) {
                    ("GET", "info") => {
                        let json = serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_string());
                        let _ = req.respond(json_response(200, &json));
                    }
                    ("POST", "register") => {
                        if let Err(code) = read_bounded_body(&mut req, MAX_LOCALSEND_CONTROL_BODY_BYTES) {
                            let msg = if code == 413 { "Payload Too Large" } else { "Bad Request" };
                            let _ = req.respond(close_text_response(code, msg));
                            continue;
                        }
                        let json = serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_string());
                        let _ = req.respond(json_response(200, &json));
                    }
                    ("POST", "vitral-clipboard") => {
                        if guard.is_pure_offline() || !clipboard_bg.lock().has_key() {
                            let _ = req.respond(tiny_http::Response::empty(403));
                            continue;
                        }
                        let body = match read_bounded_body(&mut req, MAX_CLIPBOARD_BODY_BYTES) {
                            Ok(body) => body,
                            Err(code) => {
                                let _ = req.respond(tiny_http::Response::empty(code));
                                continue;
                            }
                        };
                        let status = if guard.is_pure_offline() {
                            403
                        } else {
                            clipboard_bg.lock().receive(&body)
                        };
                        let _ = req.respond(tiny_http::Response::empty(status));
                    }
                    ("POST", "prepare-upload") => {
                        let body = match read_bounded_body(&mut req, MAX_LOCALSEND_CONTROL_BODY_BYTES) {
                            Ok(b) => b,
                            Err(code) => {
                                let msg = if code == 413 { "Payload Too Large" } else { "Bad Request" };
                                let _ = req.respond(close_text_response(code, msg));
                                continue;
                            }
                        };
                        let Ok(parsed) = serde_json::from_slice::<PrepareUploadRequest>(&body) else {
                            let _ = req.respond(json_response(400, r#"{"message":"Invalid JSON"}"#));
                            continue;
                        };
                        let _ = &parsed.info;
                        if parsed.files.is_empty() {
                            let _ = req.respond(tiny_http::Response::empty(204));
                            continue;
                        }
                        if sessions.lock().len() >= 256 {
                            let _ = req.respond(tiny_http::Response::empty(503));
                            continue;
                        }
                        let sid = format!("ls-sess-{}", sess_seq.fetch_add(1, Ordering::Relaxed));
                        let mut tokens = HashMap::new();
                        for fid in parsed.files.keys() {
                            tokens.insert(fid.clone(), format!("tok-{sid}-{fid}"));
                        }
                        let resp_obj = PrepareUploadResponse {
                            session_id: sid.clone(),
                            files: tokens.clone(),
                        };
                        sessions.lock().insert(
                            sid,
                            ServerSession {
                                files: parsed.files,
                                tokens,
                                uploaded: HashSet::new(),
                                cancelled: false,
                            },
                        );
                        let json = serde_json::to_string(&resp_obj).unwrap_or_default();
                        let _ = req.respond(json_response(200, &json));
                    }
                    ("POST", "upload") => {
                        let body = match read_bounded_body(&mut req, MAX_LOCALSEND_UPLOAD_BODY_BYTES) {
                            Ok(b) => b,
                            Err(code) => {
                                let msg = if code == 413 { "Payload Too Large" } else { "Bad Request" };
                                let _ = req.respond(close_text_response(code, msg));
                                continue;
                            }
                        };
                        let sid = query.get("sessionId").map(String::as_str).unwrap_or("");
                        let fid = query.get("fileId").map(String::as_str).unwrap_or("");
                        let tok = query.get("token").map(String::as_str).unwrap_or("");
                        if sid.is_empty() || fid.is_empty() || tok.is_empty() {
                            let _ = req.respond(json_response(400, r#"{"message":"Missing params"}"#));
                            continue;
                        }
                        let mut sess_map = sessions.lock();
                        let Some(sess) = sess_map.get_mut(sid) else {
                            let _ = req.respond(json_response(403, r#"{"message":"Invalid session"}"#));
                            continue;
                        };
                        if sess.cancelled || sess.tokens.get(fid).map(String::as_str) != Some(tok) {
                            let _ = req.respond(json_response(403, r#"{"message":"Invalid token"}"#));
                            continue;
                        }
                        if sess.uploaded.contains(fid) {
                            let _ = req.respond(json_response(409, r#"{"message":"Already uploaded"}"#));
                            continue;
                        }
                        let Some(meta) = sess.files.get(fid) else {
                            let _ = req.respond(json_response(400, r#"{"message":"Unknown fileId"}"#));
                            continue;
                        };
                        if body.len() != meta.size {
                            let _ = req.respond(json_response(400, r#"{"message":"Size mismatch"}"#));
                            continue;
                        }
                        let actual_sha = sha256_hex(&body);
                        if let Some(ref exp_sha) = meta.sha256 {
                            if !exp_sha.trim().is_empty()
                                && actual_sha != exp_sha.trim().to_ascii_lowercase()
                            {
                                let _ = req.respond(json_response(422, r#"{"message":"SHA-256 mismatch"}"#));
                                continue;
                            }
                        }
                        sess.uploaded.insert(fid.to_string());
                        let file_name = if meta.file_name.is_empty() {
                            fid.to_string()
                        } else {
                            meta.file_name.clone()
                        };
                        let mut inbox = rec_bg.lock();
                        if inbox.len() >= 128 || inbox.iter().map(|item| item.size).sum::<usize>()
                            .saturating_add(body.len()) > MAX_LOCALSEND_UPLOAD_BODY_BYTES {
                            sess.uploaded.remove(fid);
                            let _ = req.respond(tiny_http::Response::empty(507));
                            continue;
                        }
                        inbox.push(ReceivedLocalSendTransfer {
                            session_id: sid.to_string(),
                            file_id: fid.to_string(),
                            file_name,
                            size: body.len(),
                            sha256_actual: actual_sha,
                            data: body,
                        });
                        if sess.files.len() == sess.uploaded.len() {
                            sess_map.remove(sid);
                        }
                        let _ = req.respond(tiny_http::Response::empty(200));
                    }
                    ("POST", "cancel") => {
                        let sid = query.get("sessionId").map(String::as_str).unwrap_or("");
                        if let Some(sess) = sessions.lock().get_mut(sid) {
                            sess.cancelled = true;
                        }
                        let _ = req.respond(tiny_http::Response::empty(200));
                    }
                    _ => {
                        let _ = req.respond(close_text_response(405, "Method Not Allowed"));
                    }
                }
            }
        });

        Ok(LocalSendReceiverHandle {
            addr,
            stop,
            received,
            clipboard,
            join: Some(join),
        })
    }

    pub async fn push_clipboard_to_peer(
        guard: &OfflineGuard,
        base_url: &str,
        entry: &ClipboardEntry,
        sync_key: &str,
    ) -> Result<(), VitralError> {
        guard.require_online("localsend_clipboard_push")?;
        if sync_key.trim().is_empty() {
            return Err(VitralError::InvalidArgument(
                "LocalSend clipboard requires a sync key".into(),
            ));
        }
        let mut url = reqwest::Url::parse(base_url)
            .map_err(|e| VitralError::InvalidArgument(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(VitralError::InvalidArgument(
                "invalid LocalSend clipboard URL".into(),
            ));
        }
        url.path_segments_mut()
            .map_err(|_| VitralError::InvalidArgument("invalid LocalSend clipboard URL".into()))?
            .pop_if_empty()
            .extend(["api", "localsend", "v2", "vitral-clipboard"]);
        let data = serde_json::to_vec(entry).map_err(|e| VitralError::Parse(e.to_string()))?;
        let encrypted = encrypt_envelope(&data, Some(sync_key))?;
        if encrypted.len() > MAX_CLIPBOARD_BODY_BYTES {
            return Err(VitralError::InvalidArgument(
                "LocalSend clipboard exceeds 256 KiB".into(),
            ));
        }
        static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
        let client = CLIENT
            .get_or_init(|| {
                reqwest::Client::builder()
                    .tls_backend_preconfigured(crate::dav::rustls_client_config().clone())
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_millis(800))
                    .pool_idle_timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| VitralError::Io(e.clone()))?;
        guard.require_online("localsend_clipboard_push")?;
        let response = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(encrypted)
            .send()
            .await
            .map_err(|e| VitralError::Io(e.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(VitralError::Io(format!(
                "LocalSend clipboard HTTP error status {}",
                response.status().as_u16()
            )));
        }
        Ok(())
    }

    pub fn announce_udp(
        guard: &OfflineGuard,
        target_host: &str,
        target_port: u16,
        info: &LocalSendDeviceInfo,
    ) -> Result<(), VitralError> {
        guard.require_online("localsend_udp_announce")?;
        let ip: Ipv4Addr = target_host
            .parse()
            .map_err(|e| VitralError::InvalidArgument(format!("invalid IPv4 host {target_host}: {e}")))?;
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .map_err(|e| VitralError::Io(format!("UDP socket create failed: {e}")))?;
        let _ = sock.set_reuse_address(true);
        if ip.is_multicast() {
            let _ = sock.set_multicast_ttl_v4(2);
            let _ = sock.set_multicast_loop_v4(true);
        }
        let bind_sa = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        sock.bind(&bind_sa.into())
            .map_err(|e| VitralError::Io(format!("UDP socket bind failed: {e}")))?;
        let target_sa = SocketAddr::V4(SocketAddrV4::new(ip, target_port));
        let payload = serde_json::to_vec(info)
            .map_err(|e| VitralError::Internal(format!("announcement serialize failed: {e}")))?;
        sock.send_to(&payload, &target_sa.into())
            .map_err(|e| VitralError::Io(format!("UDP send_to failed: {e}")))?;
        Ok(())
    }

    pub fn register_mdns(
        guard: &OfflineGuard,
        info: &LocalSendDeviceInfo,
    ) -> Result<ServiceDaemon, VitralError> {
        guard.require_online("localsend_mdns_register")?;
        let mdns = ServiceDaemon::new()
            .map_err(|e| VitralError::Io(format!("mDNS daemon start failed: {e}")))?;
        let host_name = format!("{}.local.", info.fingerprint);
        let props = [
            ("alias", info.alias.as_str()),
            ("version", info.version.as_str()),
            ("fingerprint", info.fingerprint.as_str()),
            ("protocol", info.protocol.as_str()),
        ];
        let svc = ServiceInfo::new(
            LOCALSEND_MDNS_TYPE,
            &info.fingerprint,
            &host_name,
            "",
            info.port,
            &props[..],
        )
        .map_err(|e| VitralError::Internal(format!("mDNS ServiceInfo failed: {e}")))?;
        mdns.register(svc.enable_addr_auto())
            .map_err(|e| VitralError::Io(format!("mDNS register failed: {e}")))?;
        Ok(mdns)
    }

    pub fn discover_mdns_peers(
        guard: &OfflineGuard,
        timeout: Duration,
    ) -> Result<Vec<RelayDevice>, VitralError> {
        guard.require_online("localsend_mdns_discover")?;
        let mdns = ServiceDaemon::new()
            .map_err(|e| VitralError::Io(format!("mDNS daemon start failed: {e}")))?;
        let rx = mdns
            .browse(LOCALSEND_MDNS_TYPE)
            .map_err(|e| VitralError::Io(format!("mDNS browse failed: {e}")))?;
        let deadline = Instant::now() + timeout;
        let mut peers = Vec::new();
        let mut seen = HashSet::new();
        while Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(rem) {
                Ok(ServiceEvent::ServiceResolved(resolved)) => {
                    let fp = resolved
                        .get_property_val_str("fingerprint")
                        .unwrap_or_else(|| resolved.get_fullname())
                        .to_string();
                    if !seen.insert(fp.clone()) {
                        continue;
                    }
                    let alias = resolved
                        .get_property_val_str("alias")
                        .unwrap_or(&fp)
                        .to_string();
                    let Some(ip) = resolved.get_addresses_v4().into_iter().next() else {
                        continue;
                    };
                    let port = resolved.get_port();
                    peers.push(RelayDevice {
                        device_id: fp,
                        device_name: alias,
                        tier: DeviceTier::Lan,
                        usage_count: 0,
                        last_used_ms: 0,
                        lan_online: Some(true),
                        wan_online: Some(false),
                        localsend_url: Some(format!("http://{ip}:{port}")),
                    });
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = mdns.shutdown();
        Ok(peers)
    }

    pub fn send_v2(
        guard: &OfflineGuard,
        base_url: &str,
        sender_id: &str,
        sender_name: &str,
        file_name: &str,
        payload_bytes: &[u8],
    ) -> Result<(String, String), VitralError> {
        guard.require_online("localsend_v2_send")?;
        let clean_base = base_url.trim().trim_end_matches('/');
        if clean_base.is_empty() {
            return Err(VitralError::InvalidArgument(
                "localsend_url cannot be empty".into(),
            ));
        }
        if !clean_base.starts_with("http://") && !clean_base.starts_with("https://") {
            return Err(VitralError::InvalidArgument(format!(
                "unsupported LocalSend URL scheme: {clean_base}"
            )));
        }
        if clean_base
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '?' || c == '#')
        {
            return Err(VitralError::InvalidArgument(format!(
                "invalid LocalSend base URL: {clean_base}"
            )));
        }
        let sha256_hex_val = sha256_hex(payload_bytes);
        let file_id = "f1";
        let size = payload_bytes.len();
        let prep_body = serde_json::json!({
            "info": {
                "alias": sender_name,
                "version": "2.1",
                "deviceModel": "Vitral",
                "deviceType": "desktop",
                "fingerprint": sender_id,
                "protocol": "http"
            },
            "files": {
                file_id: {
                    "id": file_id,
                    "fileName": file_name,
                    "size": size,
                    "fileType": "application/octet-stream",
                    "sha256": sha256_hex_val
                }
            }
        });
        let prep_bytes = serde_json::to_vec(&prep_body)
            .map_err(|e| VitralError::Internal(format!("prepare-upload encode failed: {e}")))?;

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .into();

        let prep_url = format!("{clean_base}/api/localsend/v2/prepare-upload");
        let mut prep_resp = agent
            .post(&prep_url)
            .header("Content-Type", "application/json")
            .send(&prep_bytes)
            .map_err(map_ureq_error)?;
        let prep_text = prep_resp
            .body_mut()
            .read_to_string()
            .map_err(|e| VitralError::Io(format!("read prepare-upload response failed: {e}")))?;
        let prep_parsed: PrepareUploadResponse = serde_json::from_str(&prep_text)
            .map_err(|e| VitralError::Parse(format!("invalid prepare-upload JSON: {e}")))?;
        if !is_safe_query_token(&prep_parsed.session_id) || !is_safe_query_token(file_id) {
            return Err(VitralError::Parse(
                "unsafe sessionId or fileId in prepare-upload response".into(),
            ));
        }
        let token = prep_parsed
            .files
            .get(file_id)
            .ok_or_else(|| VitralError::Parse("missing file token in prepare-upload".into()))?;
        if !is_safe_query_token(token) {
            return Err(VitralError::Parse(
                "unsafe file token in prepare-upload response".into(),
            ));
        }

        let upload_url = format!(
            "{clean_base}/api/localsend/v2/upload?sessionId={}&fileId={file_id}&token={token}",
            prep_parsed.session_id
        );
        if let Err(err) = agent
            .post(&upload_url)
            .header("Content-Type", "application/octet-stream")
            .send(payload_bytes)
        {
            let cancel_url = format!(
                "{clean_base}/api/localsend/v2/cancel?sessionId={}",
                prep_parsed.session_id
            );
            let _ = agent.post(&cancel_url).send_empty();
            return Err(map_ureq_error(err));
        }

        Ok((prep_parsed.session_id, sha256_hex_val))
    }
}

pub fn send_via_relay(
    guard: &OfflineGuard,
    devices: &mut [RelayDevice],
    device_id: &str,
    payload_or_path: &str,
    is_file_path: bool,
) -> Result<RelaySendResult, VitralError> {
    send_via_relay_with_config(
        guard,
        devices,
        device_id,
        payload_or_path,
        is_file_path,
        None,
        "vitral-local-device",
        "Vitral-Host",
    )
}

#[allow(clippy::too_many_arguments)]
pub fn send_via_relay_with_config(
    guard: &OfflineGuard,
    devices: &mut [RelayDevice],
    device_id: &str,
    payload_or_path: &str,
    is_file_path: bool,
    dav_cfg: Option<&DavConfig>,
    local_device_id: &str,
    local_device_name: &str,
) -> Result<RelaySendResult, VitralError> {
    guard.require_online("relay_send")?;
    if device_id.trim().is_empty() || payload_or_path.is_empty() {
        return Err(VitralError::InvalidArgument(
            "device_id and payload_or_path must be non-empty".into(),
        ));
    }

    let (payload_bytes, file_name) = if is_file_path {
        let p = Path::new(payload_or_path);
        let bytes = std::fs::read(p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                VitralError::NotFound(format!("relay file not found: {payload_or_path}"))
            } else {
                VitralError::Io(format!("read relay file {payload_or_path} failed: {e}"))
            }
        })?;
        let fname = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("payload.bin")
            .to_string();
        (bytes, fname)
    } else {
        (payload_or_path.as_bytes().to_vec(), "note.txt".to_string())
    };

    let target_dev = devices.iter().find(|d| d.device_id == device_id).cloned();
    let use_lan = target_dev
        .as_ref()
        .is_some_and(|d| d.effective_tier() == DeviceTier::Lan && d.localsend_url.is_some());

    let result = if use_lan {
        let dev = target_dev.as_ref().expect("checked");
        let base_url = dev.localsend_url.as_deref().expect("checked");
        let (session_id, sha256) = LocalSendTransport::send_v2(
            guard,
            base_url,
            local_device_id,
            local_device_name,
            &file_name,
            &payload_bytes,
        )?;
        RelaySendResult {
            transfer_id: session_id,
            device_id: device_id.to_string(),
            transport: if is_file_path {
                "localsend_v2_file".to_string()
            } else {
                "localsend_v2_text".to_string()
            },
            sha256,
            status: "sent".to_string(),
        }
    } else if let Some(cfg) = dav_cfg {
        let seq = RELAY_TX_SEQ.fetch_add(1, Ordering::Relaxed);
        let now_ms = RetentionEngine::now_epoch_ms();
        let safe_dev: String = device_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let transfer_id = format!("tx-{now_ms}-{seq}-{safe_dev}");
        upload_koofr_relay(
            guard,
            cfg,
            local_device_id,
            device_id,
            &transfer_id,
            &file_name,
            is_file_path,
            &payload_bytes,
        )?
    } else {
        return Err(VitralError::NotFound(format!(
            "no LAN LocalSend URL or Koofr WebDAV endpoint available for target '{device_id}'"
        )));
    };

    let now_ms = RetentionEngine::now_epoch_ms();
    if let Some(dev) = devices.iter_mut().find(|d| d.device_id == device_id) {
        dev.usage_count = dev.usage_count.saturating_add(1);
        dev.last_used_ms = now_ms.max(dev.last_used_ms.saturating_add(1));
    }
    sort_devices_mru(devices);

    Ok(result)
}

fn map_ureq_error(err: ureq::Error) -> VitralError {
    match err {
        ureq::Error::StatusCode(422) => {
            VitralError::Integrity("LocalSend SHA-256 verification failed (HTTP 422)".into())
        }
        ureq::Error::StatusCode(code) => {
            VitralError::Io(format!("LocalSend HTTP error status {code}"))
        }
        other => VitralError::Io(format!("LocalSend transport error: {other}")),
    }
}

fn json_response(status: u16, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let hdr = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("valid header");
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(hdr)
}

fn close_text_response(status: u16, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let conn = tiny_http::Header::from_bytes(&b"Connection"[..], &b"close"[..])
        .expect("valid header");
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(conn)
}

fn parse_query_params(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(k.to_string(), v.to_string());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::ClipboardPayload;
    use crate::VitralCore;

    fn clipboard_entry() -> ClipboardEntry {
        ClipboardEntry {
            id: "lan-copy".into(),
            content: "剪贴板内容".into(),
            mime_type: "text/plain".into(),
            source_device: "sender".into(),
            timestamp_ms: 10,
            pinned: false,
            shortcut_code: None,
        }
    }

    #[test]
    fn clipboard_endpoint_authenticates_and_enforces_offline() {
        let guard = OfflineGuard::new(false);
        let receiver = LocalSendTransport::start_receiver(
            &guard,
            "127.0.0.1:0",
            LocalSendDeviceInfo::new("receiver", "Receiver", 0),
        )
        .unwrap();
        let key = "shared-clipboard-key";
        receiver.clipboard.lock().sync_key = Some(key.into());
        let url = receiver.base_url();
        let entry = clipboard_entry();
        let send =
            |guard, key| {
                crate::dav::network_runtime().unwrap().block_on(
                    LocalSendTransport::push_clipboard_to_peer(guard, &url, &entry, key),
                )
            };

        let wrong = send(&guard, "wrong-key").unwrap_err();
        assert!(wrong.to_string().contains("403"));
        assert!(receiver.clipboard.lock().entries.is_empty());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .into();
        let endpoint = format!("{url}/api/localsend/v2/vitral-clipboard");
        assert!(matches!(
            agent.post(&endpoint).send(b"plaintext".as_slice()),
            Err(ureq::Error::StatusCode(403))
        ));
        assert!(matches!(
            agent
                .post(&endpoint)
                .send(&vec![0u8; MAX_CLIPBOARD_BODY_BYTES + 1]),
            Err(ureq::Error::StatusCode(413))
        ));

        send(&guard, key).unwrap();
        assert_eq!(receiver.clipboard.lock().entries, [entry.clone()]);
        guard.set_pure_offline(true);
        assert!(matches!(
            send(&guard, key),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(send(&OfflineGuard::new(false), key)
            .unwrap_err()
            .to_string()
            .contains("403"));
        assert_eq!(receiver.clipboard.lock().entries, [entry]);
    }

    #[test]
    fn clipboard_push_reaches_known_lan_peer_without_webdav() {
        let key = "shared-clipboard-key";
        let receiver = VitralCore::init(
            &serde_json::json!({
                "device_id": "receiver", "sync_key": key
            })
            .to_string(),
        )
        .unwrap();
        let transport = LocalSendTransport::start_receiver(
            &receiver.offline_guard,
            "127.0.0.1:0",
            LocalSendDeviceInfo::new("receiver", "Receiver", 0),
        )
        .unwrap();
        let url = transport.base_url();
        let session = DiscoverySession {
            receiver: transport,
            mdns: ServiceDaemon::new().unwrap(),
        };
        session.set_clipboard_key(Some(key.into()));
        *receiver.lan_session.lock() = Some(session);
        let sender = VitralCore::init(
            &serde_json::json!({
                "device_id": "sender", "sync_key": key,
                "known_devices": [{
                    "device_id": "receiver", "device_name": "Receiver", "tier": "lan",
                    "lan_online": true, "localsend_url": url
                }]
            })
            .to_string(),
        )
        .unwrap();
        let expected = clipboard_entry();
        sender.clipboard_push(ClipboardPayload {
            id: Some(expected.id.clone()),
            content: expected.content.clone(),
            mime_type: expected.mime_type.clone(),
            source_device: String::new(),
            timestamp_ms: expected.timestamp_ms,
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while receiver.clipboard_lan_revision() == 0 {
            assert!(
                Instant::now() < deadline,
                "clipboard push never reached the LAN receiver"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(receiver.quick_paste_state().current_copy, Some(expected));
        assert!(receiver.clipboard_state().ring.is_empty());
    }


}
