//! Optional local models. The development assets tree remains a read-only fallback.
use crate::{eng::EngineConfig, error::VitralError, offline_guard::OfflineGuard};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_CATALOG_URL: &str =
    "https://github.com/ZZZdragondYNGPHX/Vitral/releases/download/models-v1/models-catalog.json";

type Result<T> = std::result::Result<T, VitralError>;
fn io(e: impl std::fmt::Display) -> VitralError {
    VitralError::Io(e.to_string())
}
fn invalid(s: impl Into<String>) -> VitralError {
    VitralError::InvalidArgument(s.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFile {
    pub rel_path: String,
    pub size: u64,
    pub sha256: String,
    pub urls: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub capability: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub engine: String,
    pub license: String,
    pub min_ram_mb: u64,
    pub recommended_ram_mb: u64,
    pub files: Vec<ModelFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub schema_version: u32,
    pub models: Vec<Model>,
}

pub fn validate_url(url: &str) -> Result<()> {
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|s| s.split('/').next())
        .unwrap_or("");
    if host.is_empty() || url.chars().any(char::is_whitespace) {
        return Err(invalid("模型目录与下载 URL 必须为 http:// 或 https://"));
    }
    Ok(())
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}
impl Catalog {
    pub fn parse(json: &str) -> Result<Self> {
        let c: Self = serde_json::from_str(json).map_err(|e| VitralError::Parse(e.to_string()))?;
        if c.schema_version != 1 {
            return Err(invalid("不支持的模型目录版本"));
        }
        let mut ids = HashSet::new();
        let mut paths = HashSet::new();
        for m in &c.models {
            if !safe_path(&m.id) || m.id.contains('/') || !ids.insert(&m.id) || m.files.is_empty() {
                return Err(invalid("模型 id 无效或重复，或文件列表为空"));
            }
            for f in &m.files {
                if !safe_path(&f.rel_path)
                    || !paths.insert(&f.rel_path)
                    || [
                        "state.json",
                        "state.json.part",
                        "catalog.json",
                        "catalog.json.part",
                        ".tmp",
                    ]
                    .iter()
                    .any(|p| f.rel_path.split('/').next() == Some(p))
                    || f.sha256.len() != 64
                    || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    || f.urls.is_empty()
                {
                    return Err(invalid("模型文件路径或 SHA-256 无效"));
                }
                for u in &f.urls {
                    validate_url(u)?;
                }
            }
        }
        Ok(c)
    }
    pub fn builtin() -> Self {
        Self::parse(include_str!("../../../scripts/models-catalog.json"))
            .expect("built-in model catalog")
    }
}

pub fn ram_tier(mb: u64) -> &'static str {
    match mb {
        0 => "unknown",
        1..6144 => "low",
        6144..12288 => "standard",
        _ => "high",
    }
}
pub fn ram_fit(mb: u64, m: &Model) -> &'static str {
    if mb == 0 || mb >= m.recommended_ram_mb {
        "recommended"
    } else if mb >= m.min_ram_mb {
        "compatible"
    } else {
        "exceeds_ram"
    }
}
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn total_ram_mb() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("MemTotal:")
                    .and_then(|s| s.split_whitespace().next())?
                    .parse::<u64>()
                    .ok()
            })
        })
        .unwrap_or(0)
        / 1024
}
#[cfg(windows)]
pub fn total_ram_mb() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        0
    } else {
        m.ullTotalPhys / 1024 / 1024
    }
}
#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
pub fn total_ram_mb() -> u64 {
    0
}

pub fn store_root(data_dir: &str, cache_dir: &str) -> PathBuf {
    if !data_dir.is_empty() {
        PathBuf::from(data_dir)
    } else if !cache_dir.is_empty() {
        PathBuf::from(cache_dir)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .unwrap_or_else(std::env::temp_dir)
            .join("vitral")
    }
    .join("model_store")
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalLlmModel {
    pub id: String,
    pub dir: PathBuf,
    pub model_file: PathBuf,
    pub is_fp16_kv: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ModelPaths {
    pub grammar: Option<PathBuf>,
    pub handwriting: Option<PathBuf>,
    pub reranker: Option<PathBuf>,
    pub voice: Option<PathBuf>,
    pub voice_qwen3_asr: Option<PathBuf>,
    pub local_llm: Option<LocalLlmModel>,
}
impl ModelPaths {
    pub fn resolve(
        store: Option<&Path>,
        assets: Option<&Path>,
        enabled: impl Fn(&str) -> bool,
    ) -> Self {
        let file = |id: &str, rel: &str| {
            if enabled(id) {
                store
                    .into_iter()
                    .chain(assets)
                    .map(|p| p.join(rel))
                    .find(|p| p.is_file())
            } else {
                None
            }
        };
        let complete_dir = |id: &str| {
            Catalog::builtin()
                .models
                .into_iter()
                .find(|m| m.id == id)
                .and_then(|m| {
                    if !enabled(id) {
                        return None;
                    }
                    store
                        .into_iter()
                        .chain(assets)
                        .find(|root| m.files.iter().all(|f| root.join(&f.rel_path).is_file()))
                        .map(|root| {
                            root.join(&m.files[0].rel_path)
                                .parent()
                                .unwrap()
                                .to_path_buf()
                        })
                })
        };
        let local_llm = [
            ("llm-qwen3-0.6b", "model_int8.onnx", false),
            ("llm-qwen3-1.7b", "model_q4f16.onnx", true),
        ]
        .into_iter()
        .find_map(|(id, name, is_fp16_kv)| {
            complete_dir(id).map(|dir| LocalLlmModel {
                id: id.into(),
                model_file: dir.join(name),
                dir,
                is_fp16_kv,
            })
        });
        Self {
            local_llm,
            voice_qwen3_asr: complete_dir("voice-qwen3-asr-0.6b"),
            grammar: file("wanxiang-grammar", "rime/wanxiang-lts-zh-hans.gram"),
            handwriting: file("handwriting-cnn", "handwriting/hw_model.int8.onnx")
                .and_then(|p| p.parent().map(Path::to_path_buf)),
            reranker: file("reranker-rbt3", "reranker/model_int8.onnx")
                .and_then(|p| p.parent().map(Path::to_path_buf)),
            voice: file(
                "voice-sensevoice-small",
                "voice/sensevoice-small/model.int8.onnx",
            )
            .and_then(|p| p.parent().map(Path::to_path_buf)),
        }
    }
    pub fn bundled(assets: Option<&Path>) -> Self {
        Self::resolve(None, assets, |_| true)
    }
    pub fn from_config(c: &EngineConfig, assets: Option<&Path>) -> Result<Self> {
        let root = store_root(&c.data_dir, &c.cache_dir);
        let state = load_state(&root)?;
        Ok(Self::resolve(Some(&root), assets, |id| {
            state.enabled.get(id) != Some(&false)
        }))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledModel {
    pub version: String,
    pub installed_at_ms: u64,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    enabled: BTreeMap<String, bool>,
    #[serde(default)]
    installed: BTreeMap<String, InstalledModel>,
}
fn load_state(root: &Path) -> Result<State> {
    match fs::read(root.join("state.json")) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| VitralError::Parse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(io(e)),
    }
}
fn replace(from: &Path, to: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(from, to).map_err(io)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let a: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            MoveFileExW(
                a.as_ptr(),
                b.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            Err(io(std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }
}
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap()).map_err(io)?;
    let part = path.with_extension("json.part");
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| VitralError::Parse(e.to_string()))?;
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&part).map_err(io)?;
        f.write_all(&bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
        replace(&part, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}
#[derive(Debug, Clone, Serialize, Default)]
pub struct Download {
    pub status: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub current_file: Option<String>,
    pub error: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ModelState {
    #[serde(flatten)]
    pub model: Model,
    pub fit: String,
    pub installed: bool,
    pub bundled: bool,
    pub enabled: bool,
    pub active: bool,
    pub download: Option<Download>,
}
#[derive(Debug, Serialize)]
pub struct ModelsState {
    pub total_ram_mb: u64,
    pub ram_tier: String,
    pub store_root: PathBuf,
    pub models: Vec<ModelState>,
}
struct Job {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
struct Inner {
    root: PathBuf,
    assets: Option<PathBuf>,
    catalog: RwLock<Catalog>,
    state: Mutex<State>,
    progress: Mutex<BTreeMap<String, Download>>,
    jobs: Mutex<BTreeMap<String, Job>>,
    operations: Mutex<()>,
}
#[derive(Clone)]
pub struct ModelManager {
    inner: Arc<Inner>,
}
impl ModelManager {
    pub fn new(c: &EngineConfig, assets: Option<&Path>) -> Result<Self> {
        let root = store_root(&c.data_dir, &c.cache_dir);
        let catalog = match fs::read_to_string(root.join("catalog.json")) {
            Ok(s) => Catalog::parse(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Catalog::builtin(),
            Err(e) => return Err(io(e)),
        };
        let state = load_state(&root)?;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                assets: assets.map(Path::to_path_buf),
                catalog: RwLock::new(catalog),
                state: Mutex::new(state),
                progress: Mutex::new(BTreeMap::new()),
                jobs: Mutex::new(BTreeMap::new()),
                operations: Mutex::new(()),
            }),
        })
    }
    pub fn paths(&self) -> ModelPaths {
        let s = self.inner.state.lock();
        ModelPaths::resolve(Some(&self.inner.root), self.inner.assets.as_deref(), |id| {
            s.enabled.get(id) != Some(&false)
        })
    }
    fn model(&self, id: &str) -> Result<Model> {
        self.inner
            .catalog
            .read()
            .models
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| invalid(format!("未知模型：{id}")))
    }
    fn installed(&self, m: &Model) -> bool {
        m.files.iter().all(|f| {
            fs::metadata(self.inner.root.join(&f.rel_path))
                .is_ok_and(|v| v.is_file() && v.len() == f.size)
        })
    }
    fn bundled(&self, m: &Model) -> bool {
        self.inner
            .assets
            .as_ref()
            .is_some_and(|p| m.files.iter().all(|f| p.join(&f.rel_path).is_file()))
    }
    pub fn models_state(&self) -> ModelsState {
        let total = total_ram_mb();
        let catalog = self.inner.catalog.read().clone();
        let s = self.inner.state.lock();
        let progress = self.inner.progress.lock();
        ModelsState {
            total_ram_mb: total,
            ram_tier: ram_tier(total).into(),
            store_root: self.inner.root.clone(),
            models: catalog
                .models
                .into_iter()
                .map(|m| {
                    let installed = self.installed(&m);
                    let bundled = self.bundled(&m);
                    let enabled = s
                        .enabled
                        .get(&m.id)
                        .copied()
                        .unwrap_or(installed || bundled);
                    ModelState {
                        fit: ram_fit(total, &m).into(),
                        installed,
                        bundled,
                        enabled,
                        active: (installed || bundled) && enabled,
                        download: progress.get(&m.id).cloned(),
                        model: m,
                    }
                })
                .collect(),
        }
    }
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let _op = self.inner.operations.lock();
        let m = self.model(id)?;
        if enabled && !self.installed(&m) && !self.bundled(&m) {
            return Err(VitralError::ModelUnavailable("尚未下载该模型".into()));
        }
        let mut s = self.inner.state.lock();
        let previous = s.enabled.clone();
        if enabled {
            self.disable_peers(&m, &mut s);
        }
        s.enabled.insert(id.into(), enabled);
        if let Err(e) = atomic_json(&self.inner.root.join("state.json"), &*s) {
            s.enabled = previous;
            return Err(e);
        }
        Ok(())
    }
    fn disable_peers(&self, model: &Model, state: &mut State) {
        for peer in &self.inner.catalog.read().models {
            if peer.capability == model.capability && peer.id != model.id {
                state.enabled.insert(peer.id.clone(), false);
            }
        }
    }
    fn cancel_job(&self, id: &str) -> Result<()> {
        let job = self.inner.jobs.lock().remove(id);
        if let Some(mut j) = job {
            j.cancel.store(true, Ordering::SeqCst);
            if let Some(h) = j.handle.take() {
                h.join()
                    .map_err(|_| VitralError::Internal("模型下载线程异常退出".into()))?;
            }
        }
        Ok(())
    }
    pub fn cancel_download(&self, id: &str) -> Result<()> {
        let _op = self.inner.operations.lock();
        self.model(id)?;
        self.cancel_job(id)
    }
    pub fn delete_model(&self, id: &str) -> Result<()> {
        let _op = self.inner.operations.lock();
        let m = self.model(id)?;
        self.cancel_job(id)?;
        for f in &m.files {
            let path = self.inner.root.join(&f.rel_path);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(e)),
            }
            let mut parent = path.parent();
            while let Some(p) = parent.filter(|p| *p != self.inner.root) {
                match fs::remove_dir(p) {
                    Ok(()) => parent = p.parent(),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                        ) =>
                    {
                        break
                    }
                    Err(e) => return Err(io(e)),
                }
            }
        }
        let mut s = self.inner.state.lock();
        s.installed.remove(id);
        s.enabled.remove(id);
        atomic_json(&self.inner.root.join("state.json"), &*s)?;
        self.inner.progress.lock().remove(id);
        Ok(())
    }
    pub fn start_download(
        &self,
        id: &str,
        guard: &OfflineGuard,
        on_complete: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<ModelsState> {
        guard.require_online("model_download")?;
        let _op = self.inner.operations.lock();
        let m = self.model(id)?;
        let downloading = self
            .inner
            .progress
            .lock()
            .get(id)
            .is_some_and(|p| p.status == "downloading");
        if downloading {
            return Ok(self.models_state());
        }
        self.cancel_job(id)?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner.progress.lock().insert(
            id.into(),
            Download {
                status: "downloading".into(),
                total_bytes: m.files.iter().map(|f| f.size).sum(),
                ..Default::default()
            },
        );
        let manager = self.clone();
        let guard = guard.clone();
        let cancelled = cancel.clone();
        let key = id.to_string();
        let handle = std::thread::spawn(move || {
            let tmp = manager.inner.root.join(".tmp").join(&m.id);
            let result = manager
                .download(&m, &tmp, &guard, &cancelled)
                .and_then(|()| on_complete());
            let cleanup = match fs::remove_dir_all(&tmp) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io(e)),
            };
            let _ = fs::remove_dir(manager.inner.root.join(".tmp"));
            let mut progress = manager.inner.progress.lock();
            let p = progress.get_mut(&m.id).unwrap();
            match result.and(cleanup) {
                Ok(()) => {
                    p.status = "completed".into();
                    p.current_file = None;
                }
                Err(e) => {
                    p.status = if cancelled.load(Ordering::SeqCst) {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into();
                    p.error = Some(e.to_string());
                }
            }
        });
        self.inner.jobs.lock().insert(
            key,
            Job {
                cancel,
                handle: Some(handle),
            },
        );
        Ok(self.models_state())
    }
    fn download(
        &self,
        m: &Model,
        tmp: &Path,
        guard: &OfflineGuard,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let check = || {
            if cancel.load(Ordering::SeqCst) {
                Err(invalid("下载已取消"))
            } else {
                guard.require_online("model_download")
            }
        };
        fs::create_dir_all(tmp).map_err(io)?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_per_read(Some(Duration::from_secs(5)))
            .build()
            .into();
        let mut completed = 0;
        for (idx, f) in m.files.iter().enumerate() {
            let part = tmp.join(format!("{idx}.part"));
            let mut last_error = invalid("无下载地址");
            let mut verified = false;
            let verify = || -> Result<()> {
                let mut input = fs::File::open(&part).map_err(io)?;
                let mut hash = Sha256::new();
                let mut size = 0;
                let mut buf = [0u8; 65536];
                loop {
                    check()?;
                    let len = input.read(&mut buf).map_err(io)?;
                    if len == 0 {
                        break;
                    }
                    size += len as u64;
                    hash.update(&buf[..len]);
                }
                if size != f.size
                    || !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&f.sha256)
                {
                    return Err(VitralError::Integrity(format!(
                        "{} 大小或 SHA-256 不匹配",
                        f.rel_path
                    )));
                }
                Ok(())
            };
            'mirrors: for url in &f.urls {
                for retry in 0..=3 {
                    check()?;
                    if retry > 0 {
                        // Keep cancellation/offline responsive throughout the backoff.
                        let until = Instant::now() + Duration::from_secs(1 << (retry - 1));
                        while Instant::now() < until {
                            check()?;
                            std::thread::sleep(
                                until
                                    .saturating_duration_since(Instant::now())
                                    .min(Duration::from_millis(50)),
                            );
                        }
                    }
                    let mut existing = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                    if existing > f.size {
                        fs::remove_file(&part).map_err(io)?;
                        existing = 0;
                    }
                    {
                        let mut progress = self.inner.progress.lock();
                        let p = progress.get_mut(&m.id).unwrap();
                        p.current_file = Some(f.rel_path.clone());
                        p.downloaded_bytes = completed + existing;
                    }
                    let attempt = (|| {
                        if existing == f.size && part.is_file() {
                            return verify();
                        }
                        let mut request = agent.get(url).header("Accept-Encoding", "identity");
                        if existing > 0 {
                            request = request.header("Range", format!("bytes={existing}-"));
                        }
                        let mut response = request.call().map_err(io)?;
                        let append = response.status() == 206 && existing > 0;
                        if response.status() != 200 && !append {
                            return Err(io(format!(
                                "unexpected HTTP status {}",
                                response.status()
                            )));
                        }
                        let mut out = fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .append(append)
                            .truncate(!append)
                            .open(&part)
                            .map_err(io)?;
                        let mut n = if append { existing } else { 0 };
                        self.inner
                            .progress
                            .lock()
                            .get_mut(&m.id)
                            .unwrap()
                            .downloaded_bytes = completed + n;
                        let mut reader = response.body_mut().as_reader();
                        let mut last_progress = Instant::now();
                        let mut buf = [0u8; 65536];
                        loop {
                            check()?;
                            let len = match reader.read(&mut buf) {
                                Ok(len) => len,
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        std::io::ErrorKind::TimedOut
                                            | std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::Interrupted
                                    ) || e
                                        .get_ref()
                                        .and_then(|e| e.downcast_ref::<ureq::Error>())
                                        .is_some_and(|e| matches!(e, ureq::Error::Timeout(_))) =>
                                {
                                    check()?;
                                    if last_progress.elapsed() >= Duration::from_secs(45) {
                                        return Err(io(e));
                                    }
                                    continue;
                                }
                                Err(e) => return Err(io(e)),
                            };
                            if len == 0 {
                                break;
                            }
                            n += len as u64;
                            if n > f.size {
                                return Err(VitralError::Integrity(format!(
                                    "{} 文件过大",
                                    f.rel_path
                                )));
                            }
                            out.write_all(&buf[..len]).map_err(io)?;
                            last_progress = Instant::now();
                            self.inner
                                .progress
                                .lock()
                                .get_mut(&m.id)
                                .unwrap()
                                .downloaded_bytes = completed + n;
                        }
                        out.sync_all().map_err(io)?;
                        if n < f.size {
                            return Err(io(format!("{} 下载中断，等待断点续传", f.rel_path)));
                        }
                        verify()
                    })();
                    match attempt {
                        Ok(()) => {
                            verified = true;
                            break 'mirrors;
                        }
                        Err(e) => {
                            check()?;
                            let corrupt = matches!(e, VitralError::Integrity(_));
                            last_error = e;
                            if corrupt {
                                fs::remove_file(&part).map_err(io)?;
                                break; // A corrupt mirror should not be retried with its bad bytes.
                            }
                        }
                    }
                }
            }
            if !verified {
                return Err(last_error);
            }
            completed += f.size;
        }
        check()?;
        for (idx, f) in m.files.iter().enumerate() {
            let dest = self.inner.root.join(&f.rel_path);
            fs::create_dir_all(dest.parent().unwrap()).map_err(io)?;
            replace(&tmp.join(format!("{idx}.part")), &dest)?;
        }
        let mut state = self.inner.state.lock();
        state.installed.insert(
            m.id.clone(),
            InstalledModel {
                version: m.version.clone(),
                installed_at_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            },
        );
        self.disable_peers(m, &mut state);
        state.enabled.insert(m.id.clone(), true);
        atomic_json(&self.inner.root.join("state.json"), &*state)
    }
    pub fn refresh_catalog(&self, url: &str, guard: &OfflineGuard) -> Result<ModelsState> {
        guard.require_online("models_refresh_catalog")?;
        validate_url(url)?;
        let _op = self.inner.operations.lock();
        if self
            .inner
            .progress
            .lock()
            .values()
            .any(|p| p.status == "downloading")
        {
            return Err(invalid("请等待下载结束后刷新模型目录"));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        let mut response = agent.get(url).call().map_err(io)?;
        let json = response.body_mut().read_to_string().map_err(io)?;
        guard.require_online("models_refresh_catalog")?;
        let catalog = Catalog::parse(&json)?;
        atomic_json(&self.inner.root.join("catalog.json"), &catalog)?;
        *self.inner.catalog.write() = catalog;
        Ok(self.models_state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use tiny_http::{Response, Server};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "vitral-model-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn config(&self) -> EngineConfig {
            EngineConfig {
                data_dir: self.0.to_string_lossy().into_owned(),
                assets_dir: self.0.join("assets").to_string_lossy().into_owned(),
                pure_offline: true,
                ..Default::default()
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn fixture(urls: Vec<String>, hash: &str, len: u64) -> Catalog {
        let mut c = Catalog::builtin();
        c.models.truncate(1);
        c.models[0].id = "test-model".into();
        c.models[0].files = vec![ModelFile {
            rel_path: "test/model.bin".into(),
            size: len,
            sha256: hash.into(),
            urls,
        }];
        c
    }
    fn manager(t: &Temp, c: &Catalog) -> ModelManager {
        let m = ModelManager::new(&t.config(), None).unwrap();
        *m.inner.catalog.write() = c.clone();
        m
    }
    fn serve(body: Vec<u8>, requests: usize) -> (String, JoinHandle<()>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            for _ in 0..requests {
                let req = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                req.respond(Response::from_data(body.clone())).unwrap();
            }
        });
        (url, handle)
    }
    fn wait(m: &ModelManager, id: &str) -> Download {
        let start = std::time::Instant::now();
        loop {
            let p = m.inner.progress.lock().get(id).unwrap().clone();
            if p.status != "downloading" {
                m.cancel_job(id).unwrap();
                return p;
            }
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn ram_boundaries_and_fit() {
        for (mb, tier) in [
            (0, "unknown"),
            (1, "low"),
            (6143, "low"),
            (6144, "standard"),
            (12287, "standard"),
            (12288, "high"),
        ] {
            assert_eq!(ram_tier(mb), tier);
        }
        let m = &Catalog::builtin().models[0];
        assert_eq!(ram_fit(0, m), "recommended");
        assert_eq!(ram_fit(4096, m), "recommended");
        assert_eq!(ram_fit(2048, m), "compatible");
        assert_eq!(ram_fit(2047, m), "exceeds_ram");
    }
    #[test]
    fn qwen_catalog_ram_and_complete_paths() {
        let c = Catalog::builtin();
        let t = Temp::new();
        for (id, min, recommended) in [
            ("voice-qwen3-asr-0.6b", 4096, 8192),
            ("llm-qwen3-0.6b", 3072, 6144),
            ("llm-qwen3-1.7b", 8192, 12288),
        ] {
            let m = c.models.iter().find(|m| m.id == id).unwrap();
            assert_eq!(ram_fit(min - 1, m), "exceeds_ram");
            assert_eq!(ram_fit(min, m), "compatible");
            assert_eq!(ram_fit(recommended, m), "recommended");
            for f in &m.files {
                let p = t.0.join(&f.rel_path);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, []).unwrap();
            }
        }
        let paths = ModelPaths::resolve(Some(&t.0), None, |id| id != "llm-qwen3-0.6b");
        assert!(paths.voice_qwen3_asr.is_some());
        let llm = paths.local_llm.unwrap();
        assert_eq!(llm.id, "llm-qwen3-1.7b");
        assert!(llm.is_fp16_kv);
        fs::remove_file(t.0.join("llm/qwen3-1.7b/tokenizer.json")).unwrap();
        assert!(
            ModelPaths::resolve(Some(&t.0), None, |id| id != "llm-qwen3-0.6b")
                .local_llm
                .is_none()
        );
    }
    #[test]
    fn capability_enable_and_download_are_exclusive() {
        for capability in ["voice", "local_llm", "grammar", "handwriting"] {
            let t = Temp::new();
            let bytes = b"model".to_vec();
            let (url, server) = serve(bytes.clone(), 1);
            let mut c = fixture(
                vec![url],
                &format!("{:x}", Sha256::digest(&bytes)),
                bytes.len() as u64,
            );
            c.models[0].capability = capability.into();
            let mut peer = c.models[0].clone();
            peer.id = "peer".into();
            peer.files[0].rel_path = "peer/model.bin".into();
            c.models.push(peer);
            let m = manager(&t, &c);
            let p = m.inner.root.join("peer/model.bin");
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, &bytes).unwrap();
            m.set_enabled("peer", true).unwrap();
            m.start_download("test-model", &OfflineGuard::new(false), || Ok(()))
                .unwrap();
            assert_eq!(wait(&m, "test-model").status, "completed");
            server.join().unwrap();
            assert_eq!(
                m.models_state().models.iter().filter(|s| s.active).count(),
                1
            );
            assert!(m.models_state().models[0].active);
            m.set_enabled("peer", true).unwrap();
            let s = m.models_state();
            assert!(!s.models[0].enabled && s.models[1].active);
            assert_eq!(
                load_state(&m.inner.root).unwrap().enabled.get("test-model"),
                Some(&false)
            );
        }
    }
    #[test]
    fn catalog_rejects_external_unsafe_paths_and_urls() {
        let c = Catalog::builtin();
        assert_eq!(c.models.len(), 7);
        Catalog::parse(&serde_json::to_string(&c).unwrap()).unwrap();
        for path in [
            "../x",
            "/tmp/x",
            "x/../y",
            "C:/x",
            "x\\..\\y",
            "state.json",
            "catalog.json",
            ".tmp/file",
        ] {
            let mut v = c.clone();
            v.models[0].files[0].rel_path = path.into();
            assert!(
                Catalog::parse(&serde_json::to_string(&v).unwrap()).is_err(),
                "{path}"
            );
        }
        for id in ["../x", "/x", "x/y", "C:\\x"] {
            let mut v = c.clone();
            v.models[0].id = id.into();
            assert!(Catalog::parse(&serde_json::to_string(&v).unwrap()).is_err());
        }
        let mut v = c.clone();
        v.schema_version = 2;
        assert!(Catalog::parse(&serde_json::to_string(&v).unwrap()).is_err());
        let mut v = c;
        v.models[0].files[0].urls = vec!["file:///tmp/x".into()];
        assert!(Catalog::parse(&serde_json::to_string(&v).unwrap()).is_err());
    }
    #[test]
    fn store_is_outside_android_bundled_model_directory() {
        assert_eq!(
            store_root("/data", "/cache"),
            PathBuf::from("/data/model_store")
        );
        assert_eq!(
            store_root("", "/cache"),
            PathBuf::from("/cache/model_store")
        );
    }
    #[test]
    fn download_mirror_hash_persistence_and_delete() {
        let t = Temp::new();
        let bytes = b"verified model".to_vec();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let (url, server) = serve(bytes.clone(), 1);
        let c = fixture(
            vec!["http://127.0.0.1:1/unavailable".into(), url],
            &hash,
            bytes.len() as u64,
        );
        let m = manager(&t, &c);
        let called = Arc::new(AtomicBool::new(false));
        let cb = called.clone();
        m.start_download("test-model", &OfflineGuard::new(false), move || {
            cb.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(wait(&m, "test-model").status, "completed");
        server.join().unwrap();
        assert!(called.load(Ordering::SeqCst));
        assert_eq!(
            fs::read(m.inner.root.join("test/model.bin")).unwrap(),
            bytes
        );
        let s = m.models_state();
        assert!(s.models[0].installed && s.models[0].active);
        let state = load_state(&m.inner.root).unwrap();
        assert_eq!(state.installed["test-model"].version, "LTS");
        m.set_enabled("test-model", false).unwrap();
        assert!(!m.models_state().models[0].active);
        m.set_enabled("test-model", true).unwrap();
        assert!(m.models_state().models[0].active);
        m.delete_model("test-model").unwrap();
        assert!(!m.inner.root.join("test").exists());
        assert!(!m.inner.root.join(".tmp/test-model").exists());
        assert!(load_state(&m.inner.root).unwrap().installed.is_empty());
        assert!(m
            .set_enabled("test-model", true)
            .unwrap_err()
            .to_string()
            .contains("尚未下载"));
    }
    #[test]
    fn rejects_hash_mismatch_and_cleans_parts() {
        let t = Temp::new();
        let (url, server) = serve(b"corrupt".to_vec(), 1);
        let m = manager(&t, &fixture(vec![url], &"0".repeat(64), 7));
        m.start_download("test-model", &OfflineGuard::new(false), || {
            panic!("invalid download completed")
        })
        .unwrap();
        let p = wait(&m, "test-model");
        server.join().unwrap();
        assert_eq!(p.status, "failed");
        assert!(p.error.unwrap().contains("SHA-256"));
        assert!(!m.inner.root.join("test/model.bin").exists());
        assert!(!m.inner.root.join(".tmp/test-model").exists());
    }
    #[test]
    fn complete_part_is_verified_and_reused_without_http() {
        let t = Temp::new();
        let bytes = b"complete verified part";
        let hash = format!("{:x}", Sha256::digest(bytes));
        let m = manager(
            &t,
            &fixture(
                vec!["http://127.0.0.1:1/unused".into()],
                &hash,
                bytes.len() as u64,
            ),
        );
        let tmp = m.inner.root.join(".tmp/test-model");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("0.part"), bytes).unwrap();
        m.start_download("test-model", &OfflineGuard::new(false), || Ok(()))
            .unwrap();
        assert_eq!(wait(&m, "test-model").status, "completed");
        assert_eq!(
            fs::read(m.inner.root.join("test/model.bin")).unwrap(),
            bytes
        );
        assert!(!tmp.exists());
    }

    #[test]
    fn ignored_range_restarts_instead_of_appending() {
        let t = Temp::new();
        let bytes = b"server ignores range";
        let hash = format!("{:x}", Sha256::digest(bytes));
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let req = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            assert_eq!(
                req.headers()
                    .iter()
                    .find(|h| h.field.equiv("Range"))
                    .unwrap()
                    .value
                    .as_str(),
                "bytes=3-"
            );
            req.respond(Response::from_data(bytes.to_vec())).unwrap();
        });
        let m = manager(&t, &fixture(vec![url], &hash, bytes.len() as u64));
        let tmp = m.inner.root.join(".tmp/test-model");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("0.part"), &bytes[..3]).unwrap();
        m.start_download("test-model", &OfflineGuard::new(false), || Ok(()))
            .unwrap();
        assert_eq!(wait(&m, "test-model").status, "completed");
        handle.join().unwrap();
        assert_eq!(
            fs::read(m.inner.root.join("test/model.bin")).unwrap(),
            bytes
        );
        assert!(!tmp.exists());
    }

    #[test]
    fn range_resume_after_interrupted_body_and_slow_reads() {
        struct InterruptedBody {
            sent: bool,
        }
        impl Read for InterruptedBody {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.sent {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "interrupted fixture",
                    ));
                }
                self.sent = true;
                let len = buf.len().min(8192);
                buf[..len].fill(42);
                Ok(len)
            }
        }
        struct DelayedBody {
            remaining: usize,
            reads: usize,
        }
        impl Read for DelayedBody {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Ok(0);
                }
                if self.reads == 1 {
                    std::thread::sleep(Duration::from_millis(5500));
                }
                self.reads += 1;
                let len = buf.len().min(self.remaining).min(8192);
                buf[..len].fill(42);
                self.remaining -= len;
                Ok(len)
            }
        }
        let t = Temp::new();
        let bytes = vec![42; 32768];
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let first = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            assert!(!first.headers().iter().any(|h| h.field.equiv("Range")));
            let _ = first.respond(Response::new(
                200.into(),
                vec![],
                InterruptedBody { sent: false },
                Some(32768),
                None,
            ));
            let second = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let range = second
                .headers()
                .iter()
                .find(|h| h.field.equiv("Range"))
                .unwrap();
            assert_eq!(range.value.as_str(), "bytes=8192-");
            second
                .respond(Response::new(
                    206.into(),
                    vec![
                        tiny_http::Header::from_bytes("Content-Range", "bytes 8192-32767/32768")
                            .unwrap(),
                    ],
                    DelayedBody {
                        remaining: 24576,
                        reads: 0,
                    },
                    Some(24576),
                    None,
                ))
                .unwrap();
        });
        let m = manager(&t, &fixture(vec![url], &hash, bytes.len() as u64));
        m.start_download("test-model", &OfflineGuard::new(false), || Ok(()))
            .unwrap();
        let result = wait(&m, "test-model");
        handle.join().unwrap();
        assert_eq!(result.status, "completed", "{:?}", result.error);
        assert_eq!(
            fs::read(m.inner.root.join("test/model.bin")).unwrap(),
            bytes
        );
        assert!(!m.inner.root.join(".tmp").exists());
    }

    struct SlowReader {
        remaining: usize,
    }
    impl Read for SlowReader {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(5));
            let n = b.len().min(4096).min(self.remaining);
            b[..n].fill(42);
            self.remaining -= n;
            Ok(n)
        }
    }
    #[test]
    fn cancel_inflight_is_idempotent_and_removes_partial_files() {
        let t = Temp::new();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let req = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let _ = req.respond(Response::new(
                200.into(),
                vec![],
                SlowReader {
                    remaining: 1024 * 1024,
                },
                Some(1024 * 1024),
                None,
            ));
        });
        let m = manager(&t, &fixture(vec![url], &"0".repeat(64), 1024 * 1024));
        m.start_download("test-model", &OfflineGuard::new(false), || {
            panic!("cancelled download completed")
        })
        .unwrap();
        let start = std::time::Instant::now();
        while m.inner.progress.lock()["test-model"].downloaded_bytes == 0 {
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        // Repeated starts reuse the running operation.
        assert_eq!(
            m.start_download("test-model", &OfflineGuard::new(false), || Ok(()))
                .unwrap()
                .models[0]
                .download
                .as_ref()
                .unwrap()
                .status,
            "downloading"
        );
        m.cancel_download("test-model").unwrap();
        m.cancel_download("test-model").unwrap();
        handle.join().unwrap();
        assert_eq!(m.inner.progress.lock()["test-model"].status, "cancelled");
        assert!(!m.inner.root.join(".tmp/test-model").exists());
        assert!(!m.inner.root.join("test/model.bin").exists());
    }
    #[test]
    fn offline_guard_blocks_download_and_catalog_refresh() {
        let t = Temp::new();
        let m = manager(&t, &Catalog::builtin());
        let guard = OfflineGuard::new(true);
        assert!(matches!(
            m.start_download("wanxiang-grammar", &guard, || Ok(())),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            m.refresh_catalog("http://127.0.0.1:1/catalog", &guard),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(!m.inner.root.exists());
    }
    #[test]
    fn catalog_refresh_persists_and_invalid_refresh_keeps_previous_catalog() {
        let t = Temp::new();
        let m = manager(&t, &Catalog::builtin());
        let c = fixture(vec!["http://localhost/model".into()], &"0".repeat(64), 1);
        let (url, server) = serve(serde_json::to_vec(&c).unwrap(), 1);
        m.refresh_catalog(&url, &OfflineGuard::new(false)).unwrap();
        server.join().unwrap();
        assert_eq!(
            ModelManager::new(&t.config(), None)
                .unwrap()
                .models_state()
                .models[0]
                .model
                .id,
            "test-model"
        );
        let (url, server) = serve(b"{\"schema_version\":99,\"models\":[]}".to_vec(), 1);
        assert!(m.refresh_catalog(&url, &OfflineGuard::new(false)).is_err());
        server.join().unwrap();
        assert_eq!(m.models_state().models.len(), 1);
    }
    #[test]
    fn resolved_paths_prefer_store_and_respect_explicit_disable() {
        let t = Temp::new();
        let a = t.0.join("assets");
        let s = t.0.join("model_store");
        for root in [&a, &s] {
            fs::create_dir_all(root.join("handwriting")).unwrap();
            fs::write(root.join("handwriting/hw_model.int8.onnx"), b"x").unwrap();
        }
        assert_eq!(
            ModelPaths::resolve(Some(&s), Some(&a), |_| true).handwriting,
            Some(s.join("handwriting"))
        );
        assert!(
            ModelPaths::resolve(Some(&s), Some(&a), |id| id != "handwriting-cnn")
                .handwriting
                .is_none()
        );
        fs::remove_file(s.join("handwriting/hw_model.int8.onnx")).unwrap();
        assert_eq!(
            ModelPaths::resolve(Some(&s), Some(&a), |_| true).handwriting,
            Some(a.join("handwriting"))
        );
    }
    #[test]
    fn commands_download_reloads_paths_and_offline_local_controls_work() {
        let t = Temp::new();
        let core = crate::VitralCore::init(&serde_json::to_string(&t.config()).unwrap()).unwrap();
        let bytes = b"model payload".to_vec();
        let (url, server) = serve(bytes.clone(), 1);
        let mut catalog = fixture(
            vec![url],
            &format!("{:x}", Sha256::digest(&bytes)),
            bytes.len() as u64,
        );
        catalog.models[0].id = "handwriting-cnn".into();
        catalog.models[0].capability = "handwriting".into();
        catalog.models[0].files[0].rel_path = "handwriting/hw_model.int8.onnx".into();
        let (url, catalog_server) = serve(serde_json::to_vec(&catalog).unwrap(), 1);
        let execute = |cmd, value: serde_json::Value| crate::commands::execute(&core, cmd, &value);
        assert!(execute(
            "models-download",
            serde_json::json!({"id":"handwriting-cnn"})
        )
        .is_err());
        core.set_pure_offline(false);
        execute("models-refresh-catalog", serde_json::json!({"url":url})).unwrap();
        catalog_server.join().unwrap();
        execute(
            "models-download",
            serde_json::json!({"id":"handwriting-cnn"}),
        )
        .unwrap();
        assert_eq!(wait(&core.models, "handwriting-cnn").status, "completed");
        server.join().unwrap();
        assert!(core.input.lock().engine.model_paths.handwriting.is_some());
        core.set_pure_offline(true);
        execute(
            "models-set-enabled",
            serde_json::json!({"id":"handwriting-cnn","enabled":false}),
        )
        .unwrap();
        assert!(core.input.lock().engine.model_paths.handwriting.is_none());
        execute(
            "models-set-enabled",
            serde_json::json!({"id":"handwriting-cnn","enabled":true}),
        )
        .unwrap();
        assert!(core.input.lock().engine.model_paths.handwriting.is_some());
        execute("models-cancel", serde_json::json!({"id":"handwriting-cnn"})).unwrap();
        execute("models-delete", serde_json::json!({"id":"handwriting-cnn"})).unwrap();
        assert!(core.input.lock().engine.model_paths.handwriting.is_none());
        assert!(!core.models.inner.root.join("handwriting").exists());
        assert_eq!(
            execute("models-state", serde_json::json!({})).unwrap()["models"][0]["active"],
            false
        );
    }

    #[test]
    fn core_reload_switches_paths_and_invalidates_cached_bundle() {
        let t = Temp::new();
        let cfg = t.config();
        let assets = PathBuf::from(&cfg.assets_dir);
        fs::create_dir_all(assets.join("handwriting")).unwrap();
        for f in [
            "hw_model.int8.onnx",
            "hw_model.classes.json",
            "cjkvi_ids.txt",
        ] {
            fs::write(assets.join("handwriting").join(f), b"[]").unwrap();
        }
        let core = crate::VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(core.models_state().models[1].active);
        let before =
            crate::hw::onnx_models::HwOnnxBundle::load_paths(&core.input.lock().engine.model_paths);
        core.models_set_enabled("handwriting-cnn", false).unwrap();
        assert!(core.input.lock().engine.model_paths.handwriting.is_none());
        core.models_set_enabled("handwriting-cnn", true).unwrap();
        let after =
            crate::hw::onnx_models::HwOnnxBundle::load_paths(&core.input.lock().engine.model_paths);
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(
            core.input.lock().engine.model_paths.handwriting,
            Some(assets.join("handwriting"))
        );
        let reopened = crate::VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(reopened.models_state().models[1].enabled);
    }
}
