mod http;

use crate::clip::ClipboardEntry;
use crate::dav::{
    network_runtime, read_http_metadata, run_on_blocking_network_thread, upload_koofr_relay,
    DavConfig,
};
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
use std::future::Future;
use std::net::TcpListener;
use std::sync::mpsc;
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
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const SESSION_TTL: Duration = Duration::from_secs(300);
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
    last_active: Instant,
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
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            j.thread().unpark();
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
    _mdns: MdnsRegistration,
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
        Ok(Self { receiver, _mdns: mdns })
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

/// Registration owns its daemon until explicit drop or the next offline transition.
pub struct MdnsRegistration {
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MdnsRegistration {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct MdnsDaemon(ServiceDaemon);

impl MdnsDaemon {
    fn new() -> Result<Self, VitralError> {
        ServiceDaemon::new().map(Self)
            .map_err(|e| VitralError::Io(format!("mDNS daemon start failed: {e}")))
    }
}

impl Drop for MdnsDaemon {
    fn drop(&mut self) {
        // These private daemons enqueue at most one registration or browse at a time.
        match self.0.shutdown() {
            Ok(shutdown) => {
                if let Err(error) = shutdown.recv_timeout(Duration::from_millis(250)) {
                    eprintln!("Vitral: mDNS shutdown acknowledgement: {error}");
                }
            }
            Err(mdns_sd::Error::DaemonShutdown) => {}
            Err(error) => eprintln!("Vitral: mDNS shutdown: {error}"),
        }
    }
}

struct ReceiverState<'a> {
    guard: &'a OfflineGuard,
    info: LocalSendDeviceInfo,
    received: Arc<Mutex<Vec<ReceivedLocalSendTransfer>>>,
    clipboard: Arc<Mutex<ClipboardInbox>>,
    sessions: Mutex<HashMap<String, ServerSession>>,
    sess_seq: AtomicU64,
    session_ttl: Duration,
}

impl ReceiverState<'_> {
    fn handle(&self, mut req: http::Request<'_>) {
        let url = std::mem::take(&mut req.url);
        let method = req.method.to_ascii_uppercase();
        let (path, query_str) = url.split_once('?').unwrap_or((&url, ""));
        let query = parse_query_params(query_str);

        let prefix = "/api/localsend/v2/";
        let endpoint = if path == "/api/vitral/v1/clipboard" {
            "vitral-clipboard"
        } else if let Some(endpoint) = path.strip_prefix(prefix) {
            endpoint.trim_matches('/')
        } else {
            let _ = req.text(404, "Not Found");
            return;
        };

        match (method.as_str(), endpoint) {
            ("GET", "info") => {
                let json = serde_json::to_string(&self.info).unwrap_or_else(|_| "{}".to_string());
                let _ = req.json(200, &json);
            }
            ("POST", "register") => {
                if let Err(code) = req.body(MAX_LOCALSEND_CONTROL_BODY_BYTES) {
                    let msg = if code == 413 { "Payload Too Large" } else { "Bad Request" };
                    let _ = req.text(code, msg);
                    return;
                }
                let json = serde_json::to_string(&self.info).unwrap_or_else(|_| "{}".to_string());
                let _ = req.json(200, &json);
            }
            ("POST", "vitral-clipboard") => {
                if self.guard.is_pure_offline() || !self.clipboard.lock().has_key() {
                    let _ = req.empty(403);
                    return;
                }
                let body = match req.body(MAX_CLIPBOARD_BODY_BYTES) {
                    Ok(body) => body,
                    Err(code) => {
                        let _ = req.empty(code);
                        return;
                    }
                };
                let status = if self.guard.is_pure_offline() {
                    403
                } else {
                    self.clipboard.lock().receive(&body)
                };
                let _ = req.empty(status);
            }
            ("POST", "prepare-upload") => {
                let body = match req.body(MAX_LOCALSEND_CONTROL_BODY_BYTES) {
                    Ok(b) => b,
                    Err(code) => {
                        let msg = if code == 413 { "Payload Too Large" } else { "Bad Request" };
                        let _ = req.text(code, msg);
                        return;
                    }
                };
                let Ok(parsed) = serde_json::from_slice::<PrepareUploadRequest>(&body) else {
                    let _ = req.json(400, r#"{"message":"Invalid JSON"}"#);
                    return;
                };
                let _ = &parsed.info;
                if parsed.files.is_empty() {
                    let _ = req.empty(204);
                    return;
                }
                let mut sessions = self.sessions.lock();
                sessions.retain(|_, s| {
                    !s.cancelled && s.uploaded.len() != s.files.len()
                        && s.last_active.elapsed() < self.session_ttl
                });
                if sessions.len() >= 256 {
                    let _ = req.empty(503);
                    return;
                }
                let sid = format!("ls-sess-{}", self.sess_seq.fetch_add(1, Ordering::Relaxed));
                let mut tokens = HashMap::new();
                for fid in parsed.files.keys() {
                    tokens.insert(fid.clone(), format!("tok-{sid}-{fid}"));
                }
                let resp_obj = PrepareUploadResponse {
                    session_id: sid.clone(),
                    files: tokens.clone(),
                };
                sessions.insert(
                    sid,
                    ServerSession {
                        files: parsed.files,
                        tokens,
                        uploaded: HashSet::new(),
                        cancelled: false,
                        last_active: Instant::now(),
                    },
                );
                let json = serde_json::to_string(&resp_obj).unwrap_or_default();
                let _ = req.json(200, &json);
            }
            ("POST", "upload") => {
                let body = match req.body(MAX_LOCALSEND_UPLOAD_BODY_BYTES) {
                    Ok(b) => b,
                    Err(code) => {
                        let msg = if code == 413 { "Payload Too Large" } else { "Bad Request" };
                        let _ = req.text(code, msg);
                        return;
                    }
                };
                let sid = query.get("sessionId").map(String::as_str).unwrap_or("");
                let fid = query.get("fileId").map(String::as_str).unwrap_or("");
                let tok = query.get("token").map(String::as_str).unwrap_or("");
                if sid.is_empty() || fid.is_empty() || tok.is_empty() {
                    let _ = req.json(400, r#"{"message":"Missing params"}"#);
                    return;
                }
                let mut sess_map = self.sessions.lock();
                if sess_map.get(sid).is_some_and(|s| s.last_active.elapsed() >= self.session_ttl) {
                    sess_map.remove(sid);
                    let _ = req.json(403, r#"{"message":"Expired session"}"#);
                    return;
                }
                let Some(sess) = sess_map.get_mut(sid) else {
                    let _ = req.json(403, r#"{"message":"Invalid session"}"#);
                    return;
                };
                if sess.cancelled || sess.tokens.get(fid).map(String::as_str) != Some(tok) {
                    let _ = req.json(403, r#"{"message":"Invalid token"}"#);
                    return;
                }
                if sess.uploaded.contains(fid) {
                    let _ = req.json(409, r#"{"message":"Already uploaded"}"#);
                    return;
                }
                let Some(meta) = sess.files.get(fid) else {
                    let _ = req.json(400, r#"{"message":"Unknown fileId"}"#);
                    return;
                };
                if body.len() != meta.size {
                    let _ = req.json(400, r#"{"message":"Size mismatch"}"#);
                    return;
                }
                use sha2::{Digest, Sha256};
                let mut hash = Sha256::new();
                for chunk in body.chunks(64 * 1024) {
                    if req.stopped() {
                        return;
                    }
                    hash.update(chunk);
                }
                let actual_sha = format!("{:x}", hash.finalize());
                if let Some(ref exp_sha) = meta.sha256 {
                    if !exp_sha.trim().is_empty()
                        && actual_sha != exp_sha.trim().to_ascii_lowercase()
                    {
                        let _ = req.json(422, r#"{"message":"SHA-256 mismatch"}"#);
                        return;
                    }
                }
                sess.uploaded.insert(fid.to_string());
                let file_name = if meta.file_name.is_empty() {
                    fid.to_string()
                } else {
                    meta.file_name.clone()
                };
                let mut inbox = self.received.lock();
                if inbox.len() >= 128 || inbox.iter().map(|item| item.size).sum::<usize>()
                    .saturating_add(body.len()) > MAX_LOCALSEND_UPLOAD_BODY_BYTES {
                    sess.uploaded.remove(fid);
                    let _ = req.empty(507);
                    return;
                }
                inbox.push(ReceivedLocalSendTransfer {
                    session_id: sid.to_string(),
                    file_id: fid.to_string(),
                    file_name,
                    size: body.len(),
                    sha256_actual: actual_sha,
                    data: body,
                });
                sess.last_active = Instant::now();
                let _ = req.empty(200);
            }
            ("POST", "cancel") => {
                let sid = query.get("sessionId").map(String::as_str).unwrap_or("");
                if let Some(sess) = self.sessions.lock().get_mut(sid) {
                    sess.cancelled = true;
                }
                let _ = req.empty(200);
            }
            _ => {
                let _ = req.text(405, "Method Not Allowed");
            }
        }
    }
}

pub struct LocalSendTransport;

impl LocalSendTransport {
    pub fn start_receiver(
        guard: &OfflineGuard,
        bind_addr: &str,
        info: LocalSendDeviceInfo,
    ) -> Result<LocalSendReceiverHandle, VitralError> {
        Self::start_receiver_with_timeout(guard, bind_addr, info, http::IDLE_TIMEOUT)
    }

    fn start_receiver_with_timeout(
        guard: &OfflineGuard,
        bind_addr: &str,
        info: LocalSendDeviceInfo,
        idle_timeout: Duration,
    ) -> Result<LocalSendReceiverHandle, VitralError> {
        Self::start_receiver_with_timeouts(guard, bind_addr, info, idle_timeout, SESSION_TTL)
    }

    fn start_receiver_with_timeouts(
        guard: &OfflineGuard,
        bind_addr: &str,
        mut info: LocalSendDeviceInfo,
        idle_timeout: Duration,
        session_ttl: Duration,
    ) -> Result<LocalSendReceiverHandle, VitralError> {
        let bind_addr = bind_addr.to_owned();
        let (ready, started) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let received = Arc::new(Mutex::new(Vec::<ReceivedLocalSendTransfer>::new()));
        let rec_bg = Arc::clone(&received);
        let clipboard = Arc::new(Mutex::new(ClipboardInbox::default()));
        let clipboard_bg = Arc::clone(&clipboard);
        let guard = guard.clone();
        let join = std::thread::spawn(move || {
            let run = || -> Result<(), VitralError> {
                let _online = guard.enter_online("localsend_start_receiver")?;
                let server = TcpListener::bind(&bind_addr).map_err(|e| {
                    VitralError::Io(format!("LocalSend bind {bind_addr} failed: {e}"))
                })?;
                let addr = server
                    .local_addr()
                    .map_err(|e| VitralError::Io(e.to_string()))?;
                server
                    .set_nonblocking(true)
                    .map_err(|e| VitralError::Io(e.to_string()))?;
                info.port = addr.port();
                let state = ReceiverState {
                    guard: &guard,
                    info,
                    clipboard: clipboard_bg,
                    received: rec_bg,
                    sessions: Mutex::default(),
                    sess_seq: AtomicU64::new(1),
                    session_ttl,
                };
                let _ = ready.send(Ok(addr));
                std::thread::scope(|scope| {
                    let mut workers = Vec::<std::thread::ScopedJoinHandle<'_, ()>>::new();
                    while !stop_bg.load(Ordering::Acquire) && !guard.is_pure_offline() {
                        workers.retain(|worker| !worker.is_finished());
                        if workers.len() < 4 {
                            match server.accept() {
                                Ok((stream, _)) => {
                                    let (stop, guard, state) = (&stop_bg, &guard, &state);
                                    workers.push(scope.spawn(move || {
                                        if let Ok(req) =
                                            http::Request::accept(stream, stop, guard, idle_timeout)
                                        {
                                            state.handle(req);
                                        }
                                    }));
                                    continue;
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                Err(_) => break,
                            }
                        }
                        std::thread::park_timeout(POLL_INTERVAL);
                    }
                    stop_bg.store(true, Ordering::Release);
                });
                Ok(())
            };
            if let Err(error) = run() {
                let _ = ready.send(Err(error));
            }
        });
        let addr = started
            .recv()
            .map_err(|e| VitralError::Internal(e.to_string()))??;

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
        let request = localsend_http_client()?
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(encrypted)
            .timeout(Duration::from_millis(800));
        let guard = guard.clone();
        tokio::task::spawn_blocking(move || {
            run_localsend_http(&guard, "localsend_clipboard_push", async {
                send_localsend_request(request).await?;
                Ok(())
            })
        })
        .await
        .map_err(|e| VitralError::Internal(e.to_string()))?
    }

    pub fn announce_udp(
        guard: &OfflineGuard,
        target_host: &str,
        target_port: u16,
        info: &LocalSendDeviceInfo,
    ) -> Result<(), VitralError> {
        let _online = guard.enter_online("localsend_udp_announce")?;
        let ip: Ipv4Addr = target_host
            .parse()
            .map_err(|e| VitralError::InvalidArgument(format!("invalid IPv4 host {target_host}: {e}")))?;
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .map_err(|e| VitralError::Io(format!("UDP socket create failed: {e}")))?;
        sock.set_nonblocking(true).map_err(|e| VitralError::Io(e.to_string()))?;
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
    ) -> Result<MdnsRegistration, VitralError> {
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
        let guard = guard.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let (stop, cancelled) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let run = || -> Result<(), VitralError> {
                let _online = guard.enter_online("localsend_mdns_register")?;
                let daemon = MdnsDaemon::new()?;
                daemon.0.register(svc.enable_addr_auto())
                    .map_err(|e| VitralError::Io(format!("mDNS register failed: {e}")))?;
                let _ = ready.send(Ok(()));
                while !guard.is_pure_offline()
                    && cancelled.recv_timeout(POLL_INTERVAL) == Err(mpsc::RecvTimeoutError::Timeout) {}
                Ok(())
            };
            if let Err(e) = run() {
                let _ = ready.send(Err(e));
            }
        });
        started.recv().map_err(|e| VitralError::Internal(e.to_string()))??;
        Ok(MdnsRegistration { stop, worker: Some(worker) })
    }

    pub fn discover_mdns_peers(
        guard: &OfflineGuard,
        timeout: Duration,
    ) -> Result<Vec<RelayDevice>, VitralError> {
        let _online = guard.enter_online("localsend_mdns_discover")?;
        let mdns = MdnsDaemon::new()?;
        let rx = mdns.0
            .browse(LOCALSEND_MDNS_TYPE)
            .map_err(|e| VitralError::Io(format!("mDNS browse failed: {e}")))?;
        let deadline = Instant::now() + timeout;
        let mut peers = Vec::new();
        let mut seen = HashSet::new();
        while Instant::now() < deadline {
            if guard.require_online("localsend_mdns_discover").is_err() {
                break;
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(rem.min(POLL_INTERVAL)) {
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
                Err(mdns_sd::RecvTimeoutError::Timeout) => continue,
                Err(mdns_sd::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = mdns.0.stop_browse(LOCALSEND_MDNS_TYPE);
        drop(mdns);
        guard.require_online("localsend_mdns_discover")?;
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

        let client = localsend_http_client()?;
        let prep_url = format!("{clean_base}/api/localsend/v2/prepare-upload");
        let prep_text = run_localsend_http(guard, "localsend_v2_prepare_upload", async {
            let request = client
                .post(prep_url)
                .header("Content-Type", "application/json")
                .body(prep_bytes);
            read_http_metadata(send_localsend_request(request).await?).await
        })?;
        let prep_parsed: PrepareUploadResponse = serde_json::from_slice(&prep_text)
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
        let upload = client
            .post(upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(payload_bytes.to_vec());
        let uploaded = run_localsend_http(guard, "localsend_v2_upload", async {
            send_localsend_request(upload).await?;
            Ok(())
        });
        if let Err(err) = uploaded {
            if !matches!(err, VitralError::PureOfflineViolation(_)) {
                let cancel_url = format!(
                    "{clean_base}/api/localsend/v2/cancel?sessionId={}",
                    prep_parsed.session_id
                );
                let _ = run_localsend_http(guard, "localsend_v2_cancel", async {
                    send_localsend_request(client.post(cancel_url)).await?;
                    Ok(())
                });
            }
            return Err(err);
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

fn localsend_http_client() -> Result<&'static reqwest::Client, VitralError> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .tls_backend_preconfigured(crate::dav::rustls_client_config().clone())
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(Duration::from_secs(15))
                .pool_idle_timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| VitralError::Io(e.clone()))
}

fn run_localsend_http<T: Send>(
    guard: &OfflineGuard,
    operation: &str,
    future: impl Future<Output = Result<T, VitralError>> + Send,
) -> Result<T, VitralError> {
    run_on_blocking_network_thread(|| {
        let _online = guard.enter_online(operation)?;
        network_runtime()?.block_on(guard.wait_online(operation, future, || Ok(())))
    })
}

async fn send_localsend_request(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, VitralError> {
    let response = request.send().await.map_err(|e| {
        VitralError::Io(format!("LocalSend transport error: {}", e.without_url()))
    })?;
    let status = response.status();
    match status.as_u16() {
        422 => Err(VitralError::Integrity(
            "LocalSend SHA-256 verification failed (HTTP 422)".into(),
        )),
        code if !status.is_success() => {
            Err(VitralError::Io(format!("LocalSend HTTP error status {code}")))
        }
        _ => Ok(response),
    }
}

#[cfg(test)]
fn json_response(status: u16, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let hdr = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("valid header");
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(hdr)
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
    fn switching_offline_drains_active_mdns_discovery() {
        let guard = OfflineGuard::new(false);
        let discovery_guard = guard.clone();
        let discovery = std::thread::spawn(move || {
            LocalSendTransport::discover_mdns_peers(&discovery_guard, Duration::from_secs(2))
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !guard.has_online_operation() {
            assert!(Instant::now() < deadline, "discovery did not enter the online gate");
            std::thread::yield_now();
        }
        // Allow the browse loop to enter its timed receive on the active daemon.
        std::thread::sleep(2 * POLL_INTERVAL);
        let start = Instant::now();
        guard.set_pure_offline(true);
        assert!(matches!(discovery.join().unwrap(), Err(VitralError::PureOfflineViolation(_))));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn receiver_closes_listener_and_accepted_connections_on_stop_or_offline() {
        use std::io::Read;
        for offline in [false, true] {
            let guard = OfflineGuard::new(false);
            let mut receiver = LocalSendTransport::start_receiver(
                &guard, "127.0.0.1:0", LocalSendDeviceInfo::new("receiver", "Receiver", 0),
            ).unwrap();
            let mut peer = std::net::TcpStream::connect(receiver.addr).unwrap();
            peer.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            let start = Instant::now();
            if offline {
                guard.set_pure_offline(true);
            } else {
                receiver.stop();
            }
            assert!(start.elapsed() < Duration::from_millis(500));
            assert!(std::net::TcpStream::connect(receiver.addr).is_err());
            match peer.read(&mut [0]) {
                Ok(0) => {}
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                result => panic!("peer still open: {result:?}"),
            }
        }
    }

    #[test]
    fn silent_connections_time_out_and_release_receiver_slots() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let guard = OfflineGuard::new(false);
        let receiver = LocalSendTransport::start_receiver_with_timeout(
            &guard,
            "127.0.0.1:0",
            LocalSendDeviceInfo::new("receiver", "Receiver", 0),
            Duration::from_millis(100),
        )
        .unwrap();
        let mut silent: Vec<_> = (0..4)
            .map(|_| TcpStream::connect(receiver.addr).unwrap())
            .collect();
        // Queue the normal request behind all four silent peers.
        let start = Instant::now();
        let mut peer = TcpStream::connect(receiver.addr).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        peer.write_all(b"GET /api/localsend/v2/info HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        peer.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        assert!(start.elapsed() < Duration::from_secs(1));
        for stream in &mut silent {
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn abandoned_upload_sessions_expire_before_capacity_check() {
        let guard = OfflineGuard::new(false);
        let ttl = Duration::from_millis(80);
        let receiver = LocalSendTransport::start_receiver_with_timeouts(
            &guard, "127.0.0.1:0", LocalSendDeviceInfo::new("receiver", "Receiver", 0),
            http::IDLE_TIMEOUT, ttl,
        ).unwrap();
        let url = format!("{}/api/localsend/v2/prepare-upload", receiver.base_url());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2))).build().into();
        let prepare = || agent.post(&url).send(br#"{"info":{},"files":{"f":{"size":4}}}"#);
        for _ in 0..256 {
            assert_eq!(prepare().unwrap().status(), 200);
        }
        std::thread::sleep(ttl + POLL_INTERVAL);
        assert_eq!(prepare().unwrap().status(), 200);
    }

    #[test]
    fn upload_rejects_expired_sessions_and_refreshes_successful_uploads() {
        let guard = OfflineGuard::new(false);
        let ttl = Duration::from_millis(300);
        let receiver = LocalSendTransport::start_receiver_with_timeouts(
            &guard, "127.0.0.1:0", LocalSendDeviceInfo::new("receiver", "Receiver", 0),
            http::IDLE_TIMEOUT, ttl,
        ).unwrap();
        let base = format!("{}/api/localsend/v2", receiver.base_url());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2))).build().into();
        let prepare = || {
            let response = agent.post(format!("{base}/prepare-upload"))
                .send(br#"{"info":{},"files":{"a":{"size":4},"b":{"size":4},"c":{"size":4}}}"#)
                .unwrap().body_mut().read_to_string().unwrap();
            serde_json::from_str::<PrepareUploadResponse>(&response).unwrap()
        };
        let prepared = prepare();
        let upload = |fid: &str| agent.post(format!("{base}/upload?sessionId={}&fileId={fid}&token={}",
            prepared.session_id, prepared.files[fid])).send(b"file");
        std::thread::sleep(Duration::from_millis(180));
        assert_eq!(upload("a").unwrap().status(), 200);
        std::thread::sleep(Duration::from_millis(180));
        assert_eq!(upload("b").unwrap().status(), 200);
        std::thread::sleep(ttl + POLL_INTERVAL);
        for _ in 0..2 {
            assert!(matches!(upload("c"), Err(ureq::Error::StatusCode(403))));
        }
        assert_eq!(receiver.received().len(), 2);
    }

    #[test]
    fn upload_checks_integrity_and_recognizes_completed_session_retries() {
        let guard = OfflineGuard::new(false);
        let receiver = LocalSendTransport::start_receiver(
            &guard, "127.0.0.1:0", LocalSendDeviceInfo::new("receiver", "Receiver", 0),
        ).unwrap();
        let base = format!("{}/api/localsend/v2", receiver.base_url());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2))).build().into();
        let prep = serde_json::json!({
            "info": {}, "files": {"f": {"size": 4, "sha256": sha256_hex(b"file")}}
        }).to_string();
        let mut input = prep.as_bytes();
        let response = agent.post(format!("{base}/prepare-upload"))
            .send(ureq::SendBody::from_reader(&mut input)).unwrap()
            .body_mut().read_to_string().unwrap();
        let prepared: PrepareUploadResponse = serde_json::from_str(&response).unwrap();
        let url = format!("{base}/upload?sessionId={}&fileId=f&token={}",
            prepared.session_id, prepared.files["f"]);
        assert!(matches!(agent.post(format!("{url}x")).send(b"file"),
            Err(ureq::Error::StatusCode(403))));
        assert!(matches!(agent.post(&url).send(b"oops"), Err(ureq::Error::StatusCode(422))));
        assert!(agent.post(&url).send(b"file").is_ok());
        assert!(matches!(agent.post(&url).send(b"file"), Err(ureq::Error::StatusCode(409))));
        assert_eq!(receiver.received().len(), 1);
    }

    #[test]
    fn offline_during_prepare_upload_blocks_upload_and_cancel() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let guard = OfflineGuard::new(false);
        let sender_guard = guard.clone();
        let sender = std::thread::spawn(move || {
            LocalSendTransport::send_v2(&sender_guard, &url, "sender", "Sender", "file", b"file")
        });
        let prepare = server.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(prepare.url(), "/api/localsend/v2/prepare-upload");
        let start = Instant::now();
        guard.set_pure_offline(true);
        assert!(start.elapsed() < Duration::from_millis(500));
        let _ = prepare.respond(json_response(
            200,
            r#"{"sessionId":"session","files":{"f1":"token"}}"#,
        ));

        let (mut uploads, mut cancels) = (0, 0);
        while let Some(request) = server.recv_timeout(Duration::from_millis(200)).unwrap() {
            let path = request.url().split('?').next().unwrap();
            match path {
                "/api/localsend/v2/upload" => uploads += 1,
                "/api/localsend/v2/cancel" => cancels += 1,
                other => panic!("unexpected LocalSend request {other}"),
            }
            let _ = request.respond(tiny_http::Response::empty(500));
        }
        assert_eq!((uploads, cancels), (0, 0));
        assert!(matches!(
            sender.join().unwrap(),
            Err(VitralError::PureOfflineViolation(_))
        ));
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
        let send = |guard, key| {
            network_runtime()
                .unwrap()
                .block_on(LocalSendTransport::push_clipboard_to_peer(guard, &url, &entry, key))
        };

        let wrong = send(&guard, "wrong-key").unwrap_err();
        assert!(wrong.to_string().contains("403"));
        assert!(receiver.clipboard.lock().entries.is_empty());
        let endpoint = format!("{url}/api/vitral/v1/clipboard");
        for (body, code) in [
            (b"plaintext".to_vec(), 403),
            (vec![0; MAX_CLIPBOARD_BODY_BYTES + 1], 413),
        ] {
            let request = localsend_http_client()
                .unwrap()
                .post(&endpoint)
                .body(body)
                .timeout(Duration::from_secs(2));
            let error = run_localsend_http(
                &guard,
                "localsend_clipboard_test",
                send_localsend_request(request),
            )
            .unwrap_err();
            assert_eq!(
                error,
                VitralError::Io(format!("LocalSend HTTP error status {code}"))
            );
        }

        send(&guard, key).unwrap();
        assert_eq!(
            receiver.clipboard.lock().entries.as_slice(),
            std::slice::from_ref(&entry)
        );
        guard.set_pure_offline(true);
        assert!(matches!(
            send(&guard, key),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(std::net::TcpStream::connect(receiver.addr).is_err());
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
        let session = DiscoverySession::start(
            &receiver.offline_guard, "receiver", "Receiver", Some(key.into()),
        ).unwrap();
        let url = format!("http://127.0.0.1:{}", session.receiver.port());
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
