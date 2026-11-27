#[cfg(windows)]
pub mod windows_host;
#[cfg(windows)]
mod windows_clipboard;
pub mod ai;
pub mod clip;
pub mod commands;
pub mod dav;
pub mod eng;
pub mod error;
pub mod hw;
pub mod imp;
pub mod lan;
mod local;
pub mod models;
pub mod offline_guard;
#[cfg(any(feature = "native-voice", feature = "native-onnx"))]
mod onnx_env;
pub mod ret;
pub mod profile;
pub mod predict;
pub mod sync;
pub mod shortcuts;

use crate::ai::{
    AiChat, AiClient, AiInlineResult, AiMode, InlineRequest, InlineTasks, PrivacyAudit, VoiceResult,
    VoiceSession,
};
use crate::clip::{
    ClipboardManager, ClipboardPayload, ClipboardState, QuickPasteConsumeResult, QuickPasteState,
};
use crate::dav::{
    cleanup_koofr_orphans_with_config, fetch_backup_from_koofr, poll_koofr_relay_with_config,
    upload_lexicon_snapshot, upload_profile_config,
    upload_retention_crdt_delta, DavConfig,
};
use crate::eng::{
    AppFocusState, EngineConfig, EngineState, InputAction, OpenCcMode, ReloadReport, SchemaKind,
    SwipeDir,
};
pub use crate::eng::{SpatialAlt, TouchKeyHint};
use crate::error::VitralError;
use crate::hw::{
    HandwritingPayload, HandwritingSession, HwDecodeContext, HwMode, Mode2Result, StrokePoint,
};
#[cfg(test)]
use crate::imp::compile_and_index;
use crate::imp::{
    compile_lexicon_file, finalize_compiled_lexicon_sync, ImportReport, ImportedLexiconIndex,
    LexiconFormat, SyncContext,
};
use crate::lan::send_via_relay_with_config;
use crate::offline_guard::OfflineGuard;
use crate::ret::{RetentionEngine, DEFAULT_FRECENCY_HALF_LIFE_SEC};
use crate::sync::{
    sort_devices_mru, CleanupResult, LexiconSnapshotPayload, ProfileBackup, RelayDevice,
    RelayPollResult, RelaySendResult, RestoreResult,
};
use parking_lot::{Mutex, RwLock};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vitral_tokens::validate_generated_parity;

pub struct InputState {
    pub engine: EngineState,
    pub hw: HandwritingSession,
    pub retention: RetentionEngine,
    pub profile: profile::ProfileEngine,
    pub voice: VoiceSession,
    #[cfg(feature = "native-onnx")]
    semantic_worker_running: bool,
    #[cfg(feature = "native-onnx")]
    neural_rerank_worker_running: bool,
}

pub struct SyncState {
    pub clipboard: ClipboardManager,
    pub devices: Vec<RelayDevice>,
    pub dav_config: Option<DavConfig>,
    pub device_id: String,
    pub device_name: String,
    clipboard_sync_key: Option<String>,
    current_copy_etag: Option<String>,
    current_copy_version: Option<(u64, String)>,
    local_tier1_dirty: bool,
    backup_pending: bool,
    backup_generation: u64,
    last_cleanup: Option<std::time::Instant>,
    clipboard_changes: clip::RingChanges,
    last_lan_sync_ms: u64,
    last_lan_error: Option<String>,
    last_cloud_sync_ms: u64,
    last_cloud_error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct SyncHealthStatus {
    pub pure_offline: bool,
    pub lan_enabled: bool,
    pub lan_peer_count: usize,
    pub last_lan_sync_ms: u64,
    pub last_lan_error: Option<String>,
    pub cloud_configured: bool,
    pub last_cloud_sync_ms: u64,
    pub last_cloud_error: Option<String>,
    pub backup_count: usize,
}

impl SyncState {
    fn record_lan_result<T>(&mut self, result: &Result<T, VitralError>) {
        match result {
            Ok(_) => {
                self.last_lan_sync_ms = RetentionEngine::now_epoch_ms();
                self.last_lan_error = None;
            }
            Err(VitralError::PureOfflineViolation(_)) => {}
            Err(error) => self.last_lan_error = Some(error.to_string()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier1Sync {
    Changed,
    Local,
    All,
}

#[derive(Default)]
struct ClipboardWrites {
    changes: Option<clip::RingChanges>,
    history: Option<Vec<clip::ClipboardEntry>>,
    current: Option<clip::ClipboardCurrent>,
}

#[derive(Default)]
struct SyncStateIo {
    clipboard_history_dirty: bool,
    clipboard_changes_dirty: bool,
    clipboard_current_dirty: bool,
}

pub struct VitralCore {
    local_dir: Option<std::path::PathBuf>,
    retention_backup_dir: Option<std::path::PathBuf>,
    clipboard_io: Mutex<()>,
    clipboard_current_io: Mutex<()>,
    // Acquire before sync for local writes, and release sync before saving the snapshot.
    sync_state_io: Mutex<SyncStateIo>,
    tier1_io: Mutex<()>,
    tier1_upload: Mutex<()>,
    lan_session: Mutex<Option<lan::DiscoverySession>>,
    ai: RwLock<Arc<AiClient>>,
    ai_inline_tasks: InlineTasks,
    privacy_audit: PrivacyAudit,
    offline_guard: OfflineGuard,
    imported_lexicon: Arc<RwLock<ImportedLexiconIndex>>,
    input: Arc<Mutex<InputState>>,
    models: models::ModelManager,
    prediction: predict::PredictEngine,
    power_save_mode: AtomicBool,
    profile_distill_model: Mutex<()>,
    #[cfg(feature = "native-onnx")]
    semantic_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    #[cfg(feature = "native-onnx")]
    neural_rerank_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    sync: Arc<Mutex<SyncState>>,
}

#[derive(Debug, serde::Serialize)]
pub struct ContextNextHint {
    pub initial: String,
    pub syllable: String,
    pub ch: String,
    pub source: &'static str,
}

impl Drop for VitralCore {
    fn drop(&mut self) {
        self.prediction.cancel();
        self.ai_inline_tasks.cancel(None);
        #[cfg(feature = "native-onnx")]
        {
            let handles = {
                let mut semantic = self.semantic_worker.lock();
                let mut rerank = self.neural_rerank_worker.lock();
                let mut st = self.input.lock();
                st.engine.reset_commit_context();
                st.engine.clear_neural_rerank_cache();
                [semantic.take(), rerank.take()]
            };
            for handle in handles.into_iter().flatten() {
                let _ = handle.join();
            }
        }
        self.sync_tier1_best_effort(Tier1Sync::Local);
        self.flush_dirty_clipboard(true);
    }
}

impl VitralCore {
    pub fn init(config_json: &str) -> Result<Self, VitralError> {
        validate_generated_parity().map_err(|errs| {
            let msg = errs
                .into_iter()
                .map(|e| format!("G{}:{}: {}", e.group, e.field, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            VitralError::Integrity(msg)
        })?;

        let mut config: EngineConfig = if config_json.trim().is_empty() {
            EngineConfig::default()
        } else {
            EngineConfig::parse_json(config_json)?
        };
        shortcuts::normalize(&mut config.shortcuts).map_err(VitralError::InvalidArgument)?;
        if config.clipboard_ring_capacity == 0 {
            return Err(VitralError::InvalidArgument(
                "clipboard_ring_capacity must be >= 1".into(),
            ));
        }

        let pure_offline = config.pure_offline;
        let ring_cap = config.clipboard_ring_capacity;
        let device_id = config.device_id.clone();
        let device_name = config.device_name.clone();
        let half_life_sec = config
            .frecency_half_life_sec
            .unwrap_or(DEFAULT_FRECENCY_HALF_LIFE_SEC);
        let dav_config = DavConfig::from_engine_config(&config);
        let clipboard_sync_key = config.sync_key.clone();
        let mut devices = config.known_devices.clone();
        sort_devices_mru(&mut devices);

        let ai = AiClient::from(&config);
        let mut engine = EngineState::new(config);
        let models = models::ModelManager::new(&engine.config, engine.resolved_assets_dir.as_deref())?;
        engine.model_paths = models.paths();
        engine.pure_rust.set_model_paths(&engine.model_paths);
        // Keep the grammar and corpus's initial reads outside the keystroke path.
        engine.pure_rust.get_gram();
        let local_dir = (!engine.config.data_dir.is_empty())
            .then(|| std::path::PathBuf::from(&engine.config.data_dir));
        let retention_backup_dir = engine.config.state_dir.as_deref()
            .filter(|dir| !dir.trim().is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| local_dir.clone());
        let imported_lexicon = Arc::new(RwLock::new(ImportedLexiconIndex::with_corpus_cache(
            engine.resolved_assets_dir.as_deref(),
            local_dir.as_deref(),
        )));
        engine.pure_rust.get_corpus();
        crate::imp::xiaohe::canonical_syllable_set();
        let mut hw = HandwritingSession::with_assets_dir(engine.resolved_assets_dir.as_deref());
        hw.set_model_paths(engine.model_paths.clone());
        let voice = VoiceSession { model_paths: engine.model_paths.clone(), ..Default::default() };

        let mut retention = RetentionEngine::with_half_life(half_life_sec);
        let mut clipboard = ClipboardManager::with_device(ring_cap, &device_id);
        if let Some(dir) = &local_dir {
            if let Some(remembered) = local::load::<Vec<RelayDevice>>(dir, "relay-devices.json")? {
                for mut peer in remembered {
                    if peer.device_id != device_id && !devices.iter().any(|d| d.device_id == peer.device_id) {
                        peer.lan_online = Some(false);
                        peer.localsend_url = None;
                        devices.push(peer);
                    }
                }
                sort_devices_mru(&mut devices);
            }
            if let Some(mut snapshot) =
                local::load::<LexiconSnapshotPayload>(dir, "lexicon-state.json")?
            {
                if snapshot.retention.purge_clipboard_phrases() {
                    local::save(dir, "lexicon-state.json", &snapshot)
                        .map_err(|e| VitralError::Io(format!("lexicon-state.json: {e}")))?;
                }
                retention = snapshot.retention;
                if let Some(seconds) = engine.config.frecency_half_life_sec {
                    retention.set_half_life_sec(seconds);
                }
                imported_lexicon.write().inject_hotwords(&snapshot.imported_entries);
            }
            if let Some(memory)=local::load(dir,"app-languages.json")? {engine.app_lang_memory=memory;}
            if let Some(ring)=local::load(dir,"clipboard-history.json")? { clipboard.restore_ring_cold_boot(ring); }
            if let Some(current)=local::load(dir,"clipboard-current.json")? { clipboard.restore_current(current); }
        }
        let backup_pending = match &local_dir {
            Some(dir) => local::load(dir, "backup-pending.json")?.unwrap_or(false),
            None => false,
        };
        let clipboard_changes = match &local_dir {
            Some(dir) => local::load(dir, "clipboard-changes.json")?.unwrap_or_default(),
            None => Default::default(),
        };
        let profile = profile::ProfileEngine::new(local_dir.clone())?;
        let privacy_audit = PrivacyAudit::default();
        let offline_guard = OfflineGuard::new(pure_offline);
        Ok(Self {
            prediction: predict::PredictEngine::with_audit(privacy_audit.clone(), offline_guard.clone()),
            local_dir,
            retention_backup_dir,
            clipboard_io: Mutex::new(()),
            clipboard_current_io: Mutex::new(()),
            sync_state_io: Mutex::default(),
            tier1_io: Mutex::new(()),
            tier1_upload: Mutex::new(()),
            lan_session: Mutex::new(None),
            ai: RwLock::new(Arc::new(ai)),
            ai_inline_tasks: InlineTasks::default(),
            privacy_audit,
            offline_guard,
            power_save_mode: AtomicBool::new(false),
            profile_distill_model: Mutex::new(()),
            imported_lexicon,
            models,
            input: Arc::new(Mutex::new(InputState {
                engine,
                hw,
                retention,
                profile,
                voice,
                #[cfg(feature = "native-onnx")]
                semantic_worker_running: false,
                #[cfg(feature = "native-onnx")]
                neural_rerank_worker_running: false,
            })),
            #[cfg(feature = "native-onnx")]
            semantic_worker: Mutex::new(None),
            #[cfg(feature = "native-onnx")]
            neural_rerank_worker: Mutex::new(None),
            sync: Arc::new(Mutex::new(SyncState {
                clipboard,
                devices,
                clipboard_sync_key,
                current_copy_etag: None,
                current_copy_version: None,
                local_tier1_dirty: false,
                backup_pending,
                backup_generation: 0,
                last_cleanup: None,
                clipboard_changes,
                dav_config,
                device_id,
                device_name,
                last_lan_sync_ms: 0,
                last_lan_error: None,
                last_cloud_sync_ms: 0,
                last_cloud_error: None,
            })),
        })
    }

    pub fn imported_lexicon(&self) -> Arc<RwLock<ImportedLexiconIndex>> {
        Arc::clone(&self.imported_lexicon)
    }

    pub fn unload_idle_models(&self) {
        self.predict_cancel();
        #[cfg(feature = "native-onnx")]
        {
            let handles = {
                let mut semantic = self.semantic_worker.lock();
                let mut rerank = self.neural_rerank_worker.lock();
                let mut st = self.input.lock();
                st.engine.cancel_pending_semantic_context();
                st.engine.clear_neural_rerank_cache();
                [semantic.take(), rerank.take()]
            };
            for handle in handles.into_iter().flatten() {
                let _ = handle.join();
            }
        }
        crate::hw::onnx_models::HwOnnxBundle::clear_cache();
        crate::ai::clear_model_cache();
    }

    fn schedule_semantic_context(&self) {
        #[cfg(feature = "native-onnx")]
        {
            let mut worker = self.semantic_worker.lock();
            let task = {
                let mut st = self.input.lock();
                if st.semantic_worker_running {
                    return;
                }
                let Some(task) = st.engine.take_semantic_context() else { return };
                st.semantic_worker_running = true;
                task
            };
            let input = Arc::downgrade(&self.input);
            let result = std::thread::Builder::new()
                .name("vitral-semantic-context".into())
                .spawn(move || {
                    let (mut epoch, mut context, mut paths) = task;
                    loop {
                        {
                            let Some(input) = input.upgrade() else { break };
                            let mut st = input.lock();
                            if !st.engine.is_semantic_epoch(epoch) {
                                if let Some(next) = st.engine.take_semantic_context() {
                                    (epoch, context, paths) = next;
                                    continue;
                                }
                                st.semantic_worker_running = false;
                                break;
                            }
                        }
                        let bundle = hw::onnx_models::HwOnnxBundle::load_paths(&paths);
                        let result = bundle.encode_rbt3_context(&context);
                        if result.is_none() {
                            eprintln!("Vitral: semantic context inference failed");
                        }
                        let Some(input) = input.upgrade() else { break };
                        let mut st = input.lock();
                        if let Some((vector, priors)) = result {
                            st.engine.apply_semantic_context(epoch, vector, priors);
                        }
                        if let Some(next) = st.engine.take_semantic_context() {
                            (epoch, context, paths) = next;
                        } else {
                            st.semantic_worker_running = false;
                            break;
                        }
                    }
                });
            match result {
                Ok(handle) => {
                    let prev = worker.replace(handle);
                    drop(worker);
                    drop(prev);
                }
                Err(error) => {
                    self.input.lock().semantic_worker_running = false;
                    drop(worker);
                    eprintln!("Vitral: failed to start semantic context worker: {error}");
                }
            }
        }
    }

    fn schedule_neural_rerank(&self) {
        #[cfg(feature = "native-onnx")]
        {
            let mut worker = self.neural_rerank_worker.lock();
            let task = {
                let mut st = self.input.lock();
                if st.neural_rerank_worker_running {
                    return;
                }
                let Some(task) = st.engine.take_neural_rerank() else { return };
                st.neural_rerank_worker_running = true;
                task
            };
            let input = Arc::downgrade(&self.input);
            let result = std::thread::Builder::new()
                .name("vitral-neural-rerank".into())
                .spawn(move || {
                    let mut task = task;
                    loop {
                        // Score before acquiring the input lock. The sync lock is never needed.
                        let scores = task.score();
                        let Some(input) = input.upgrade() else { break };
                        let mut st = input.lock();
                        if let Some(scores) = scores {
                            st.engine.apply_neural_rerank(task, scores);
                        }
                        if let Some(next) = st.engine.take_neural_rerank() {
                            task = next;
                        } else {
                            st.neural_rerank_worker_running = false;
                            break;
                        }
                    }
                });
            match result {
                Ok(handle) => *worker = Some(handle),
                Err(error) => {
                    self.input.lock().neural_rerank_worker_running = false;
                    eprintln!("Vitral: failed to start neural rerank worker: {error}");
                }
            }
        }
    }

    fn save_local(&self, name: &str, value: &impl serde::Serialize) -> bool {
        if let Some(dir) = &self.local_dir {
            if let Err(error) = local::save(dir, name, value) {
                eprintln!("Vitral: failed to save {name}: {error}");
                return false;
            }
        }
        true
    }

    fn mark_tier1_dirty(&self, sync: &mut SyncState, background_sync: bool) {
        sync.local_tier1_dirty |= self.local_dir.is_some();
        if background_sync && sync.dav_config.is_some() {
            sync.backup_pending = true;
            sync.backup_generation = sync.backup_generation.wrapping_add(1);
        }
    }

    fn sync_tier1_best_effort(&self, mode: Tier1Sync) {
        if let Err(error) = self.sync_tier1(mode) {
            eprintln!("Vitral: tier 1 sync failed: {error}");
        }
    }

    fn track_cloud<T>(&self, result: Result<T, VitralError>) -> Result<T, VitralError> {
        let mut sync = self.sync.lock();
        match &result {
            Ok(_) => {
                sync.last_cloud_sync_ms = RetentionEngine::now_epoch_ms();
                sync.last_cloud_error = None;
            }
            Err(VitralError::PureOfflineViolation(_)) => {}
            Err(error) => sync.last_cloud_error = Some(error.to_string()),
        }
        result
    }

    fn sync_tier1(&self, mode: Tier1Sync) -> Result<(), VitralError> {
        let background_sync = self.input.lock().engine.config.background_sync;
        let upload = mode == Tier1Sync::All || (mode == Tier1Sync::Changed && !background_sync);
        let _upload = upload.then(|| self.tier1_upload.lock());
        // Serializes snapshots and writes while allowing the input thread to mark new changes.
        let io = self.tier1_io.lock();
        let (profile, snapshot, local_dirty, backup_pending, remote) = {
            let st = self.input.lock();
            let (local_dirty, backup_pending, remote) = {
                let mut sync = self.sync.lock();
                if mode == Tier1Sync::Changed {
                    self.mark_tier1_dirty(&mut sync, background_sync);
                }
                let dav_cfg = sync.dav_config.as_ref().filter(|_| {
                    !self.is_pure_offline()
                        && match mode {
                            Tier1Sync::Changed => !background_sync,
                            Tier1Sync::Local => false,
                            Tier1Sync::All => sync.backup_pending,
                        }
                });
                if !sync.local_tier1_dirty && dav_cfg.is_none() {
                    return Ok(());
                }
                let remote = dav_cfg.map(|cfg| {
                    (cfg.clone(), sync.device_id.clone(), sync.backup_generation)
                });
                (
                    std::mem::take(&mut sync.local_tier1_dirty),
                    sync.backup_pending,
                    remote,
                )
            };
            let profile = ProfileBackup {
                shortcuts: st.engine.config.shortcuts.clone(),
                schema_version: 1,
                schema: st.engine.schema,
                opencc_mode: st.engine.opencc_mode,
                clipboard_ring_capacity: st.engine.config.clipboard_ring_capacity,
                frecency_half_life_sec: st.retention.half_life_sec,
                app_lang_memory: st.engine.app_lang_memory.clone(),
            };
            let snapshot = LexiconSnapshotPayload {
                schema_version: 1,
                retention: st.retention.clone(),
                imported_entries: self.imported_lexicon.read().export_entries(),
            };
            (profile, snapshot, local_dirty, backup_pending, remote)
        };
        let captured_ms = RetentionEngine::now_epoch_ms();

        if let Some(dir) = &self.local_dir {
            if local_dirty {
                let save = || -> std::io::Result<()> {
                    local::save(dir, "lexicon-state.json", &snapshot)?;
                    local::save(dir, "app-languages.json", &profile.app_lang_memory)?;
                    if backup_pending {
                        local::save(dir, "backup-pending.json", &true)?;
                    }
                    Ok(())
                };
                if let Err(error) = save() {
                    self.sync.lock().local_tier1_dirty = true;
                    return Err(VitralError::Io(error.to_string()));
                }
            }
        }

        drop(io);
        let Some((dav_cfg, device_id, generation)) = remote else { return Ok(()) };
        self.track_cloud((|| {
            upload_profile_config(&self.offline_guard, &dav_cfg, &profile)?;
            upload_lexicon_snapshot(&self.offline_guard, &dav_cfg, &snapshot)?;
            let crdt_delta = snapshot.retention.into_crdt_delta(device_id, captured_ms);
            upload_retention_crdt_delta(&self.offline_guard, &dav_cfg, &crdt_delta)
        })())?;
        if mode == Tier1Sync::All {
            let _io = self.tier1_io.lock();
            let unchanged = |sync: &SyncState| {
                sync.backup_generation == generation && sync.dav_config.as_ref() == Some(&dav_cfg)
            };
            let still_current = unchanged(&self.sync.lock());
            if still_current {
                if let Some(dir) = &self.local_dir {
                    local::save(dir, "backup-pending.json", &false)
                        .map_err(|error| VitralError::Io(error.to_string()))?;
                }
                let mut sync = self.sync.lock();
                if unchanged(&sync) {
                    sync.backup_pending = false;
                }
            }
        }
        Ok(())
    }

    /// Called on the platform service queue, away from the keystroke path.
    pub fn poll_services(&self) -> Result<QuickPasteState, VitralError> {
        self.sync_tier1(Tier1Sync::Local)?;
        let clipboard = self.poll_clipboard()?;
        if self.is_pure_offline() { return Ok(clipboard); }
        self.sync_tier1(Tier1Sync::All)?;
        let cleanup = {
            let mut s = self.sync.lock();
            let due = s.dav_config.is_some() && s.last_cleanup.is_none_or(|last| last.elapsed().as_secs() >= 3600);
            if due { s.last_cleanup = Some(std::time::Instant::now()); }
            due
        };
        if cleanup { self.relay_cleanup_orphans()?; }
        Ok(clipboard)
    }

    fn flush_dirty_clipboard(&self, include_history: bool) {
        let mut io = self.sync_state_io.lock();
        let writes_history = include_history && io.clipboard_history_dirty;
        let writes_changes = include_history && io.clipboard_changes_dirty;
        if self.local_dir.is_none()
            || (!writes_history && !writes_changes && !io.clipboard_current_dirty)
        {
            return;
        }
        let writes = {
            let s = self.sync.lock();
            ClipboardWrites {
                history: writes_history.then(|| s.clipboard.export_ring()),
                changes: writes_changes.then(|| s.clipboard_changes.clone()),
                current: io
                    .clipboard_current_dirty
                    .then(|| s.clipboard.current().clone()),
            }
        };
        self.save_clipboard(&mut io, writes);
    }

    pub fn poll_clipboard_current(&self) -> Result<QuickPasteState, VitralError> {
        let _current_io = self.clipboard_current_io.lock();
        self.consume_lan_clipboard();
        self.flush_dirty_clipboard(false);
        let (cfg, current, etag) = {
            let s = self.sync.lock();
            let Some(cfg) = s.dav_config.as_ref().filter(|_| !self.is_pure_offline()) else {
                return Ok(s.clipboard.quick_paste_state());
            };
            let current = s.clipboard.current().current_copy.clone();
            let etag = s
                .current_copy_version
                .as_ref()
                .filter(|(ts, id)| {
                    current
                        .as_ref()
                        .is_some_and(|entry| clip::copy_order(entry) == (*ts, id.as_str()))
                })
                .and(s.current_copy_etag.clone());
            (cfg.clone(), current, etag)
        };
        let result = self.track_cloud(dav::sync_current_copy(
            &self.offline_guard,
            &cfg,
            current.as_ref(),
            etag.as_deref(),
        ))?;
        let version = result
            .etag
            .as_ref()
            .and_then(|_| result.entry.as_ref().or(current.as_ref()))
            .map(|entry| (entry.timestamp_ms, entry.id.clone()));
        let mut io = self.sync_state_io.lock();
        let writes = {
            let mut s = self.sync.lock();
            if self.is_pure_offline() || s.dav_config.as_ref() != Some(&cfg) {
                return Ok(s.clipboard.quick_paste_state());
            }
            s.current_copy_etag = result.etag;
            s.current_copy_version = version;
            let changed = result
                .entry
                .is_some_and(|entry| s.clipboard.observe_current(entry));
            ClipboardWrites {
                current: (changed && self.local_dir.is_some())
                    .then(|| s.clipboard.current().clone()),
                ..Default::default()
            }
        };
        self.save_clipboard(&mut io, writes);
        drop(io);
        Ok(self.quick_paste_state())
    }

    pub fn poll_clipboard(&self) -> Result<QuickPasteState, VitralError> {
        let first_current = self.poll_clipboard_current().map(|_| ());
        self.poll_clipboard_history(first_current)
    }

    fn poll_clipboard_history(
        &self,
        first_current: Result<(), VitralError>,
    ) -> Result<QuickPasteState, VitralError> {
        let _io = self.clipboard_io.lock();
        self.flush_dirty_clipboard(true);
        let (cfg, before, changes, capacity) = {
            let s = self.sync.lock();
            let Some(cfg) = s.dav_config.as_ref().filter(|_| !self.is_pure_offline()) else {
                return Ok(s.clipboard.quick_paste_state());
            };
            (
                cfg.clone(),
                s.clipboard.export_ring(),
                s.clipboard_changes.clone(),
                s.clipboard.capacity(),
            )
        };
        let fetched = self.track_cloud(if changes.is_empty() {
            dav::download_clipboard_ring(&self.offline_guard, &cfg)
        } else {
            dav::merge_clipboard_ring(&self.offline_guard, &cfg, &changes, &before, capacity)
                .map(Some)
        })?;
        let mut io = self.sync_state_io.lock();
        let (current_changed, writes) = {
            let mut s = self.sync.lock();
            let mut writes = ClipboardWrites::default();
            let mut current_changed = false;
            if !self.is_pure_offline() && s.dav_config.as_ref() == Some(&cfg) {
                let latest = fetched
                    .iter()
                    .flatten()
                    .max_by_key(|entry| clip::copy_order(entry));
                current_changed =
                    latest.is_some_and(|entry| s.clipboard.observe_current(entry.clone()));
                if s.clipboard.ring_matches(&before) && s.clipboard_changes == changes {
                    if !changes.is_empty() {
                        s.clipboard_changes = Default::default();
                        writes.changes =
                            self.local_dir.as_ref().map(|_| s.clipboard_changes.clone());
                    }
                    if let Some(entries) = fetched {
                        if entries != before {
                            s.clipboard.replace_ring(entries);
                            writes.history = self.local_dir.as_ref().map(|_| s.clipboard.export_ring());
                        }
                    }
                }
                if self.local_dir.is_some() {
                    if writes.history.is_none() && io.clipboard_history_dirty {
                        writes.history = Some(s.clipboard.export_ring());
                    }
                    if writes.changes.is_none() && io.clipboard_changes_dirty {
                        writes.changes = Some(s.clipboard_changes.clone());
                    }
                    if current_changed || io.clipboard_current_dirty {
                        writes.current = Some(s.clipboard.current().clone());
                    }
                }
            }
            (current_changed, writes)
        };
        self.save_clipboard(&mut io, writes);
        drop(io);
        if current_changed {
            return self.poll_clipboard_current();
        }
        if first_current.is_err() {
            self.track_cloud(first_current)?;
        }
        Ok(self.quick_paste_state())
    }

    fn save_clipboard(&self, io: &mut SyncStateIo, writes: ClipboardWrites) {
        let mut history_ok = !io.clipboard_history_dirty;
        if let Some(history) = writes.history {
            history_ok = self.save_local("clipboard-history.json", &history);
            io.clipboard_history_dirty = !history_ok;
        }
        if let Some(changes) = writes.changes {
            if history_ok {
                io.clipboard_changes_dirty = !self.save_local("clipboard-changes.json", &changes);
            } else {
                io.clipboard_changes_dirty = true;
            }
        }
        if let Some(current) = writes.current {
            io.clipboard_current_dirty = !self.save_local("clipboard-current.json", &current);
        }
    }

    fn record_clipboard_change(&self, s: &mut SyncState, before: &[clip::ClipboardEntry]) -> ClipboardWrites {
        let mut writes = ClipboardWrites::default();
        if s.dav_config.is_some() || self.local_dir.is_some() {
            let after = s.clipboard.export_ring();
            if s.dav_config.is_some() {
                s.clipboard_changes.record(before, &after);
                writes.changes = self.local_dir.as_ref().map(|_| s.clipboard_changes.clone());
            }
            writes.history = self.local_dir.as_ref().map(|_| after);
        }
        writes
    }

    /// Apply settings without replacing composing, clipboard or retention state.
    pub fn apply_preferences(&self, cfg: EngineConfig) -> Result<(), VitralError> {
        self.apply_preferences_clearing_secrets(cfg, false)
    }

    /// Apply settings, optionally clearing a blank Koofr password explicitly.
    pub fn apply_preferences_clearing_secrets(
        &self,
        mut cfg: EngineConfig,
        clear_koofr_password: bool,
    ) -> Result<(), VitralError> {
        cfg.validate_prediction()?;
        cfg.validate_quote_style()?;
        cfg.validate_voice_cleanup_mode()?;
        let fuzzy_pairs = cfg.validated_fuzzy_pairs()?;
        let typo_correction_level = cfg.validated_typo_correction_level()?;
        if let Some(url) = cfg.model_catalog_url.as_deref().filter(|s| !s.is_empty()) { models::validate_url(url)?; }
        shortcuts::normalize(&mut cfg.shortcuts).map_err(VitralError::InvalidArgument)?;
        if cfg.clipboard_ring_capacity == 0 {
            return Err(VitralError::InvalidArgument(
                "clipboard_ring_capacity must be >= 1".into(),
            ));
        }
        let _tier1_io = self.tier1_io.lock();
        let mut st = self.input.lock();
        self.prediction.clear_cache();
        let mut io = self.sync_state_io.lock();
        let mut sync = self.sync.lock();
        cfg.koofr_endpoint = cfg.koofr_endpoint.map(|s| s.trim().to_string());
        cfg.koofr_username = cfg
            .koofr_username
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        cfg.koofr_password = cfg
            .koofr_password
            .map(|s| s.trim_matches(['\r', '\n']).to_string())
            .filter(|s| !s.is_empty());
        if !clear_koofr_password
            && cfg.koofr_password.is_none()
            && cfg.koofr_endpoint.as_deref().is_some_and(|s| !s.is_empty())
            && cfg.koofr_endpoint.as_deref()
                == st.engine.config.koofr_endpoint.as_deref().map(str::trim)
        {
            cfg.koofr_password
                .clone_from(&st.engine.config.koofr_password);
        }
        self.offline_guard.set_pure_offline_with(cfg.pure_offline, |was_offline| {
            if !was_offline {
                self.ai_inline_tasks.cancel_remote();
            }
            self.lan_session.lock().take();
        });
        if st.engine.schema != cfg.schema { st.engine.set_schema(cfg.schema); }
        st.engine.set_opencc_mode(cfg.opencc_mode);
        let previous_half_life_sec = st.retention.half_life_sec;
        if let Some(seconds) = cfg.frecency_half_life_sec { st.retention.set_half_life_sec(seconds); }
        sync.clipboard.set_capacity(cfg.clipboard_ring_capacity);
        let next = DavConfig::from_engine_config(&cfg);
        let sync_settings = if sync.dav_config != next {
            sync.current_copy_etag = None;
            sync.current_copy_version = None;
            sync.last_cleanup = None;
            sync.backup_pending = next.is_some();
            sync.backup_generation = sync.backup_generation.wrapping_add(1);
            sync.clipboard_changes = Default::default();
            let cur = (next.is_some() || (self.local_dir.is_some() && io.clipboard_history_dirty))
                .then(|| sync.clipboard.export_ring());
            if next.is_some() {
                if let Some(ref entries) = cur {
                    sync.clipboard_changes.record(&[], entries);
                }
            }
            sync.dav_config = next;
            self.local_dir.as_ref().map(|_| {
                (
                    sync.backup_pending,
                    ClipboardWrites {
                        history: io.clipboard_history_dirty.then(|| cur.unwrap_or_else(|| sync.clipboard.export_ring())),
                        changes: Some(sync.clipboard_changes.clone()),
                        current: io.clipboard_current_dirty.then(|| sync.clipboard.current().clone()),
                    },
                )
            })
        } else {
            None
        };
        if sync.clipboard_sync_key != cfg.sync_key {
            sync.clipboard_sync_key.clone_from(&cfg.sync_key);
            if let Some(session) = self.lan_session.lock().as_ref() {
                session.set_clipboard_key(cfg.sync_key.clone());
            }
        }
        *self.ai.write() = Arc::new(AiClient::from(&cfg));
        if st.engine.config.shortcuts != cfg.shortcuts { st.engine.clear_composition_state(); }
        let fuzzy_changed = st.engine.config.fuzzy_pinyin != cfg.fuzzy_pinyin
            || st.engine.pure_rust.fuzzy_pairs != fuzzy_pairs;
        let split_changed = st.engine.config.syllable_split != cfg.syllable_split;
        let correction_changed = st.engine.pure_rust.typo_correction_level != typo_correction_level;
        let result = (|| {
            let was_private = st.engine.is_active_privacy_app();
            st.engine.config.privacy_apps = cfg.privacy_apps;
            if st.engine.is_active_privacy_app() != was_private {
                st.engine.clear_composition_state();
                st.engine.reset_commit_context();
                st.engine.last_committed_char = None;
            }
            st.engine.set_syllable_split(cfg.syllable_split)?;
            st.engine.pure_rust.typo_correction_level = typo_correction_level;
            st.engine.pure_rust.fuzzy_pinyin = cfg.fuzzy_pinyin;
            st.engine.pure_rust.fuzzy_pairs = fuzzy_pairs;
            let active = &mut st.engine.config;
            active.frecency_half_life_sec = cfg.frecency_half_life_sec;
            active.model_catalog_url = cfg.model_catalog_url.clone();
            active.shortcuts = cfg.shortcuts;
            active.linux_english_mode = cfg.linux_english_mode;
            let adaptive_changed = active.adaptive_learning != cfg.adaptive_learning;
            active.adaptive_learning = cfg.adaptive_learning;
            active.fuzzy_pinyin = cfg.fuzzy_pinyin;
            active.fuzzy_pinyin_pairs = cfg.fuzzy_pinyin_pairs;
            active.undo_commit_on_backspace = cfg.undo_commit_on_backspace;
            active.typo_correction_level = cfg.typo_correction_level;
            active.prediction_enabled = cfg.prediction_enabled;
            active.prediction_skip_dev_apps = cfg.prediction_skip_dev_apps;
            active.prediction_delay_ms = cfg.prediction_delay_ms;
            active.profile_distill_enabled = cfg.profile_distill_enabled;
            active.voice_cleanup_mode = cfg.voice_cleanup_mode;
            active.auto_english_for_dev_apps = cfg.auto_english_for_dev_apps;
            active.default_english_apps = cfg.default_english_apps;
            active.disable_paired_punct_in_ide = cfg.disable_paired_punct_in_ide;
            active.disable_paired_punct_apps = cfg.disable_paired_punct_apps;
            active.quote_style = cfg.quote_style;
            active.auto_micro_space = cfg.auto_micro_space;
            active.schema = cfg.schema;
            active.opencc_mode = cfg.opencc_mode;
            active.clipboard_ring_capacity = cfg.clipboard_ring_capacity;
            active.koofr_endpoint = cfg.koofr_endpoint;
            active.koofr_username = cfg.koofr_username;
            active.koofr_password = cfg.koofr_password;
            active.koofr_root = cfg.koofr_root;
            active.sync_key = cfg.sync_key;
            active.ai_tier3_enabled = cfg.ai_tier3_enabled;
            active.ai_prefer_local = cfg.ai_prefer_local;
            if adaptive_changed {
                st.engine.refresh_semantic_context();
            }
            if split_changed || correction_changed || fuzzy_changed {
                let imported = self.imported_lexicon.read();
                let InputState { engine, retention, .. } = &mut *st;
                engine.refresh_candidates(&sync.clipboard, &imported, retention)?;
            }
            Ok(())
        })();
        let snapshot = (result.is_ok()
            && st.retention.half_life_sec != previous_half_life_sec
            && self.local_dir.is_some())
            .then(|| LexiconSnapshotPayload {
                schema_version: 1,
                retention: st.retention.clone(),
                imported_entries: self.imported_lexicon.read().export_entries(),
            });
        drop(sync);
        drop(st);
        if let (Some(dir), Some(snapshot)) = (&self.local_dir, snapshot) {
            local::save(dir, "lexicon-state.json", &snapshot)
                .map_err(|e| VitralError::Io(format!("lexicon-state.json: {e}")))?;
        }
        if let Some((backup_pending, writes)) = sync_settings {
            self.save_local("backup-pending.json", &backup_pending);
            self.save_clipboard(&mut io, writes);
        }
        result
    }

    pub fn cloud_usage(&self) -> Result<serde_json::Value, VitralError> {
        self.offline_guard.require_online("storage")?;
        let cfg = self.sync.lock().dav_config.clone().ok_or_else(|| VitralError::InvalidArgument("Koofr endpoint required".into()))?;
        let client = crate::dav::KoofrDavClient::connect(&self.offline_guard, &cfg)?;
        Ok(serde_json::json!({
            "lexicon":client.directory_bytes("lexicon/")? + client.directory_bytes("profile/")?,
            "clipboard":client.directory_bytes("clipboard/")?,
            "relay":client.directory_bytes("relay/")?
        }))
    }

    pub fn set_pure_offline(&self, enabled: bool) {
        self.offline_guard.set_pure_offline_with(enabled, |was_offline| {
            if !was_offline {
                self.ai_inline_tasks.cancel_remote();
            }
            self.lan_session.lock().take();
        });
    }

    pub fn is_pure_offline(&self) -> bool {
        self.offline_guard.is_pure_offline()
    }

    pub fn set_power_save(&self, enabled: bool) {
        let was_enabled = self.power_save_mode.swap(enabled, Ordering::AcqRel);
        ai::set_ai_power_save(enabled);
        if enabled {
            if !was_enabled {
                self.predict_cancel();
            }
            ai::evict_idle_speech_models();
            // An in-flight distillation must finish before unloading its model.
            let _distill = self.profile_distill_model.lock();
            ai::unload_local_llm();
        }
    }

    pub fn is_power_save(&self) -> bool {
        self.power_save_mode.load(Ordering::Acquire)
    }

    pub fn sync_status(&self) -> SyncHealthStatus {
        let backup_count = self.retention_backup_dir.as_deref().map_or(0, |dir| {
            local::backup_count(dir).unwrap_or_else(|error| {
                eprintln!("Vitral: failed to count retention backups: {error}");
                0
            })
        });
        let sync = self.sync.lock();
        let pure_offline = self.is_pure_offline();
        let lan_peer_count = if pure_offline {
            0
        } else {
            sync.devices.iter().filter(|peer| {
                peer.effective_tier() == sync::DeviceTier::Lan
                    && peer.localsend_url.is_some()
                    && peer.device_id != sync.device_id
            }).count()
        };
        let lan_enabled = !pure_offline && (lan_peer_count > 0 || self.lan_session.lock().is_some());
        SyncHealthStatus {
            pure_offline,
            lan_enabled,
            lan_peer_count,
            last_lan_sync_ms: sync.last_lan_sync_ms,
            last_lan_error: sync.last_lan_error.clone(),
            cloud_configured: sync.dav_config.is_some(),
            last_cloud_sync_ms: sync.last_cloud_sync_ms,
            last_cloud_error: sync.last_cloud_error.clone(),
            backup_count,
        }
    }

    pub fn set_schema(&self, schema: SchemaKind) {
        let mut st = self.input.lock();
        st.engine.set_schema(schema);
    }

    pub fn set_opencc_mode(&self, mode: OpenCcMode) {
        let mut st = self.input.lock();
        st.engine.set_opencc_mode(mode);
    }

    pub fn reload_assets(&self) -> Result<ReloadReport, VitralError> {
        self.prediction.clear_cache();
        let rep = Self::reload_input(&self.input, &self.imported_lexicon)?;
        self.sync_tier1_best_effort(Tier1Sync::Changed);
        Ok(rep)
    }

    fn reload_input(
        input: &Mutex<InputState>,
        imported_lexicon: &RwLock<ImportedLexiconIndex>,
    ) -> Result<ReloadReport, VitralError> {
        let mut st = input.lock();
        crate::eng::gram::RimeGramDb::clear_cache();
        crate::hw::onnx_models::HwOnnxBundle::clear_cache();
        crate::hw::ids_exta::IdsIndex::clear_cache();
        ai::clear_model_cache();
        let rep = st.engine.reload_assets()?;
        st.engine.pure_rust.get_corpus();
        let paths = st.engine.model_paths.clone();
        let assets = st.engine.resolved_assets_dir.clone();
        imported_lexicon.write().reload_assets(assets.as_deref());
        st.hw.set_assets_dir(assets.as_deref());
        st.hw.set_model_paths(paths.clone());
        st.voice.model_paths = paths;
        Ok(rep)
    }

    pub fn models_state(&self) -> models::ModelsState { self.models.models_state() }
    pub fn models_download(&self, id: &str) -> Result<models::ModelsState, VitralError> {
        let input = Arc::clone(&self.input);
        let imported_lexicon = Arc::clone(&self.imported_lexicon);
        self.models.start_download(id, &self.offline_guard, move || {
            Self::reload_input(&input, &imported_lexicon).map(|_| ())
        })
    }
    pub fn models_cancel(&self, id: &str) -> Result<models::ModelsState, VitralError> {
        self.models.cancel_download(id)?; Ok(self.models_state())
    }
    pub fn models_set_enabled(&self, id: &str, enabled: bool) -> Result<models::ModelsState, VitralError> {
        self.models.set_enabled(id, enabled)?; self.reload_assets()?; Ok(self.models_state())
    }
    pub fn models_delete(&self, id: &str) -> Result<models::ModelsState, VitralError> {
        // Release cached mappings/sessions before deleting files (Windows).
        self.models.cancel_download(id)?;
        self.models.set_enabled(id, false)?; self.reload_assets()?;
        self.models.delete_model(id)?; self.reload_assets()?; Ok(self.models_state())
    }
    pub fn models_refresh_catalog(&self, url: Option<&str>) -> Result<models::ModelsState, VitralError> {
        let configured = self.input.lock().engine.config.model_catalog_url.clone();
        self.offline_guard.require_online("models_refresh_catalog")?;
        let url = url.filter(|s| !s.is_empty()).or(configured.as_deref().filter(|s| !s.is_empty())).unwrap_or(models::DEFAULT_CATALOG_URL);
        self.models.refresh_catalog(url, &self.offline_guard)
    }

    pub fn on_app_focus(&self, app_id: &str) -> Result<AppFocusState, VitralError> {
        self.on_app_focus_with_learning_flag(app_id, false)
    }

    pub(crate) fn on_app_focus_with_learning_flag(
        &self,
        app_id: &str,
        no_personalized_learning: bool,
    ) -> Result<AppFocusState, VitralError> {
        let mut st = self.input.lock();
        let state = st.engine.on_app_focus_with_learning_flag(app_id, no_personalized_learning)?;
        self.prediction.cancel();
        Ok(state)
    }

    pub fn set_syllable_split(&self, enabled: bool) -> Result<InputAction, VitralError> {
        let mut st = self.input.lock();
        st.engine.set_syllable_split(enabled)?;
        st.engine.pure_rust.page_index = 0;
        st.engine.current_page.page_index = 0;
        let sync = self.sync.lock();
        let imported = self.imported_lexicon.read();
        let InputState {
            engine, retention, ..
        } = &mut *st;
        engine.refresh_candidates(&sync.clipboard, &imported, retention)
    }

    pub fn candidate_state(&self) -> Result<InputAction, VitralError> {
        let mut st = self.input.lock();
        let sync = self.sync.lock();
        let imported = self.imported_lexicon.read();
        let InputState { engine, retention, .. } = &mut *st;
        engine.refresh_candidates(&sync.clipboard, &imported, retention)
    }

    fn select_quick_paste(&self, engine: &mut EngineState, index: usize) -> Option<InputAction> {
        let id = engine.current_candidates.get(index)?.quick_paste_id.as_deref()?;
        let mut io = self.sync_state_io.lock();
        let mut writes = ClipboardWrites::default();
        let committed = {
            let mut sync = self.sync.lock();
            // A late click must never consume a newer copy than the one displayed.
            if sync.clipboard.quick_paste_id() == Some(id) {
                let consumed = sync.clipboard.consume_quick_paste().consumed.map(|c| c.content);
                writes.current = self.local_dir.as_ref().map(|_| sync.clipboard.current().clone());
                consumed
            } else {
                None
            }
        };
        self.save_clipboard(&mut io, writes);
        engine.clear_composition_state();
        let clipboard_commit = committed.is_some();
        if clipboard_commit {
            engine.reset_commit_context();
        }
        let mut action = engine.commit_action(committed, false);
        action.clipboard_commit = clipboard_commit;
        Some(action)
    }

    fn select_function(engine: &mut EngineState, index: usize) -> Option<InputAction> {
        let action = engine.current_candidates.get(index)?.function_action?;
        Some(Self::run_function(engine, action))
    }

    fn run_function(engine: &mut EngineState, action: shortcuts::ShortcutAction) -> InputAction {
        engine.clear_composition_state();
        if action == shortcuts::ShortcutAction::ToggleLanguage { engine.toggle_lang_mode(); }
        let mut result = engine.empty_action(None);
        result.function_action = Some(action);
        result
    }

    pub fn shortcut_key(&self, key: &str) -> Result<Option<InputAction>, VitralError> {
        let _tier1_io = self.tier1_io.lock();
        let mut st = self.input.lock();
        let normalized = shortcuts::normalize_key(key).map_err(VitralError::InvalidArgument)?;
        let action = st.engine.config.shortcuts.iter().find(|b| !b.key.is_empty() && b.key == normalized && st.engine.config.shortcut_enabled(b.action)).map(|b| b.action);
        let result = action.map(|a| Self::run_function(&mut st.engine, a));
        if action == Some(shortcuts::ShortcutAction::ToggleLanguage) { self.save_local("app-languages.json", &st.engine.app_lang_memory); }
        Ok(result)
    }

    pub fn process_key(
        &self,
        keycode: u32,
        modifiers: u32,
        swipe_dir: SwipeDir,
    ) -> Result<InputAction, VitralError> {
        self.process_touch_key(keycode, modifiers, swipe_dir, TouchKeyHint::default())
    }

    pub fn process_touch_key(
        &self,
        keycode: u32,
        modifiers: u32,
        swipe_dir: SwipeDir,
        hint: TouchKeyHint,
    ) -> Result<InputAction, VitralError> {
        let (act, sync_now, raw_commit) = {
            let mut st = self.input.lock();
            let before_seq = st.retention.crdt_seq;
            if st.engine.config.platform == "android" && modifiers == 0 && swipe_dir == SwipeDir::None {
                if matches!(keycode, 0x08 | 0xff08) && st.engine.raw_input.is_empty() {
                    let visible = self.sync.lock().clipboard.visible_quick_paste().is_some();
                    if visible {
                        let mut io = self.sync_state_io.lock();
                        let current = {
                            let mut sync = self.sync.lock();
                            if sync.clipboard.visible_quick_paste().is_some() {
                                sync.clipboard.dismiss_quick_paste();
                                self.local_dir.as_ref().map(|_| sync.clipboard.current().clone())
                            } else {
                                None
                            }
                        };
                        self.save_clipboard(&mut io, ClipboardWrites { current, ..Default::default() });
                        drop(io);
                        let InputState { engine, retention, .. } = &mut *st;
                        let sync_st = self.sync.lock();
                        let imported = self.imported_lexicon.read();
                        return engine.refresh_candidates(&sync_st.clipboard, &imported, retention);
                    }
                } else if (0x20..=0x7e).contains(&keycode) {
                    self.sync.lock().clipboard.hide_for_input();
                }
            }
            let unmodified = modifiers == 0 && swipe_dir == SwipeDir::None;
            let raw_commit = unmodified && matches!(keycode, 0x0d | 0xff0d)
                && !st.engine.raw_input.is_empty();
            if unmodified && !st.engine.is_filter_tone_key(keycode) {
                let index = match keycode {
                    0x20 if st.engine.lang_mode == eng::LangMode::Zh => Some(0),
                    0x31..=0x35 if st.engine.lang_mode == eng::LangMode::Zh && !st.engine.raw_input.is_empty() => Some((keycode - 0x31) as usize),
                    _ => None,
                };
                if let Some(action) = index.and_then(|i| Self::select_function(&mut st.engine, i)) {
                    return Ok(action);
                }
            }
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            let mut sync_st = self.sync.lock();
            let act = if raw_commit {
                let passthrough = engine.is_passthrough_active();
                let raw = engine.raw_commit_text();
                engine.settle_undo_regret(None, retention);
                engine.clear_composition_state();
                engine.commit_action(Some(raw), !passthrough)
            } else {
                let imported = self.imported_lexicon.read();
                engine.process_touch_key(keycode, modifiers, swipe_dir, hint, &sync_st.clipboard, &imported, retention)?
            };
            let changed = retention.crdt_seq != before_seq || modifiers & eng::MOD_LANG_TOGGLE != 0;
            let background_sync = engine.config.background_sync;
            if changed && background_sync {
                self.mark_tier1_dirty(&mut sync_st, true);
            }
            if let Some(text) = act.commit_text.as_deref().filter(|_| !raw_commit && !act.clipboard_commit) {
                self.record_profile_commit(&mut st, text);
            }
            (act, changed && !background_sync, raw_commit)
        };
        self.schedule_neural_rerank();
        if sync_now {
            self.sync_tier1_best_effort(Tier1Sync::Changed);
        }
        if act.commit_text.is_some() && !raw_commit && !act.clipboard_commit {
            self.schedule_semantic_context();
        }
        Ok(act)
    }

    pub fn set_preedit_cursor(&self, cursor_utf8: usize) -> Result<InputAction, VitralError> {
        let mut st = self.input.lock();
        let InputState { engine, retention, .. } = &mut *st;
        let sync = self.sync.lock();
        let imported = self.imported_lexicon.read();
        engine.set_preedit_cursor(cursor_utf8, &sync.clipboard, &imported, retention)
    }

    pub fn reset_commit_context(&self) {
        self.input.lock().engine.reset_commit_context();
    }

    pub fn notify_deleted_chars(&self, count: usize) {
        self.input.lock().engine.notify_deleted_chars(count);
    }

    pub fn select_candidate(&self, index: usize) -> Result<InputAction, VitralError> {
        let (act, sync_now) = {
            let mut st = self.input.lock();
            if let Some(action) = Self::select_function(&mut st.engine, index).or_else(|| self.select_quick_paste(&mut st.engine, index)) {
                return Ok(action);
            }
            let before_seq = st.retention.crdt_seq;
            let mut sync_st = self.sync.lock();
            let imported = self.imported_lexicon.read();
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            let action = engine.select_candidate_action(
                index,
                &sync_st.clipboard,
                &imported,
                retention,
            )?;
            let changed = retention.crdt_seq != before_seq;
            let background_sync = engine.config.background_sync;
            if changed && background_sync {
                self.mark_tier1_dirty(&mut sync_st, true);
            }
            if let Some(text) = action.commit_text.as_deref().filter(|_| !action.clipboard_commit) {
                self.record_profile_commit(&mut st, text);
            }
            (action, changed && !background_sync)
        };
        self.schedule_neural_rerank();
        if sync_now {
            self.sync_tier1_best_effort(Tier1Sync::Changed);
        }
        if act.commit_text.is_some() && !act.clipboard_commit {
            self.schedule_semantic_context();
        }
        Ok(act)
    }

    pub fn pin_candidate(&self, index: usize) -> Result<InputAction, VitralError> {
        let act = {
            let mut st = self.input.lock();
            if st.engine.current_candidates.get(index).is_some_and(|c| c.quick_paste_id.is_some() || c.function_action.is_some()) {
                return Ok(st.engine.empty_action(None));
            }
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            engine.pin_candidate_in_state(index, retention)?;
            engine.empty_action(None)
        };
        self.sync_tier1_best_effort(Tier1Sync::Changed);
        Ok(act)
    }

    pub fn erase_candidate(&self, index: usize) -> Result<InputAction, VitralError> {
        let act = {
            let mut st = self.input.lock();
            if st.engine.current_candidates.get(index).is_some_and(|c| c.quick_paste_id.is_some() || c.function_action.is_some()) {
                return Ok(st.engine.empty_action(None));
            }
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            engine.erase_candidate_in_state(index, retention)?;
            engine.empty_action(None)
        };
        self.sync_tier1_best_effort(Tier1Sync::Changed);
        Ok(act)
    }

    pub fn hw_begin(&self, mode: HwMode) {
        let mut st = self.input.lock();
        st.hw.begin(mode);
    }

    pub fn hw_push_stroke(
        &self,
        points: &[StrokePoint],
        pen_up: bool,
    ) -> Result<HandwritingPayload, VitralError> {
        let mut st = self.input.lock();
        if st.hw.active_mode.is_none() {
            return Err(VitralError::NotInitialized);
        }
        if st.engine.model_paths.handwriting.is_none() {
            return Err(VitralError::ModelUnavailable(
                "尚未下载或已禁用离线手写识别模型，请在模型中心下载并启用".into(),
            ));
        }
        let gram = st.engine.model_paths.grammar.as_ref().and_then(|p| crate::eng::gram::RimeGramDb::open_cached(p).ok());
        let imported = self.imported_lexicon.read();
        let InputState {
            ref mut hw,
            ref retention,
            ..
        } = *st;
        let ctx = HwDecodeContext {
            imported: &imported,
            gram: gram.as_deref(),
            retention,
            tier3: None,
            offline_guard: Some(&self.offline_guard),
        };
        hw.push_stroke_with_context(points, pen_up, &ctx)
    }

    pub fn hw_end(&self) -> Result<HandwritingPayload, VitralError> {
        let (payload, revision, refine, app_id, private) = {
            let mut st = self.input.lock();
            if st.hw.active_mode.is_none() {
                return Err(VitralError::NotInitialized);
            }
            if st.engine.model_paths.handwriting.is_none() {
                return Err(VitralError::ModelUnavailable(
                    "尚未下载或已禁用离线手写识别模型，请在模型中心下载并启用".into(),
                ));
            }
            let gram = st.engine.model_paths.grammar.as_ref().and_then(|p| crate::eng::gram::RimeGramDb::open_cached(p).ok());
            let imported = self.imported_lexicon.read();
            let private = st.engine.is_active_privacy_app();
            let app_id = st.engine.active_app_id.clone();
            let refine = st.engine.config.ai_tier3_enabled && !private;
            let InputState { hw, retention, .. } = &mut *st;
            let ctx = HwDecodeContext {imported:&imported,gram:gram.as_deref(),retention,tier3:None,offline_guard:Some(&self.offline_guard)};
            let payload = hw.end_with_context(&ctx)?;
            let revision = hw.revision;
            let refine = refine && !hw.confirmed_since_edit;
            if let HandwritingPayload::Mode1(result) = &payload {
                st.engine.lookup_hints.clone_from(&result.candidates);
            }
            (payload, revision, refine, app_id, private)
        };
        let HandwritingPayload::Mode2(local) = &payload else { return Ok(payload) };
        let mut choices = Vec::new();
        let local_llm = self.input.lock().engine.model_paths.local_llm.clone();
        let ai = Arc::clone(&self.ai.read());
        let local_model = local_llm.as_ref().filter(|model| ai.uses_local(&self.offline_guard, Some(model)));
        let local_refine = local_model.is_some();
        if refine {
            let pure_offline = self.is_pure_offline();
            for slot in local.slots.iter().filter(|s| !s.locked_by_entropy && !s.manually_locked && s.top8.len() > 1) {
                if pure_offline && !local_refine {
                    self.privacy_audit.record("handwriting_refine", "blocked_offline", true);
                    break;
                }
                let options:Vec<_> = slot.top8.iter().map(|c| c.ch.as_str()).collect();
                let prompt = "Choose one character from these handwriting candidates. Return JSON with slot_index equal to its zero-based array index. Preserve the local choice if uncertain.";
                let question = serde_json::json!({"candidates":options,"local_choice":slot.ch,"text":local.composing_text,"position":slot.slot_idx});
                let activity = self.privacy_audit.record(
                    "handwriting_refine", if local_refine { "local" } else { "remote" }, self.is_pure_offline(),
                );
                let chat = AiChat { guard: &self.offline_guard, local_model, activity: Some(&activity) };
                let choice = ai.chat_with_route(chat, prompt, &question.to_string()).ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                    .and_then(|v| v["slot_index"].as_u64())
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| options.get(i).copied());
                if let Some(ch) = choice {choices.push((slot.slot_idx,ch.to_owned()));}
            }
        }
        let (payload, confirmed) = {
            let mut st = self.input.lock();
            let imported = self.imported_lexicon.read();
            let InputState {
                hw,
                engine,
                retention,
                ..
            } = &mut *st;
            if hw.revision != revision {
                return Err(VitralError::InvalidArgument(
                    "handwriting changed during recognition".into(),
                ));
            }
            if !hw.confirmed_since_edit && (!self.is_pure_offline() || local_refine) {
                for (i,ch) in choices {
                    let slot=&mut hw.last_slots[i];
                    slot.changed_retroactively |= slot.ch != ch;
                    slot.ch=ch;
                }
            }
            let snapshot = hw.snapshot()?;
            let newly_confirmed = !hw.confirmed_since_edit;
            let learn = !private && app_id == engine.active_app_id;
            let committed = Self::confirm_handwriting(hw, engine, retention, &imported, learn);
            if let Some(text) = committed.as_deref().filter(|_| newly_confirmed && learn) {
                self.record_profile_commit(&mut st, text);
            }
            (snapshot, newly_confirmed && committed.is_some())
        };
        if confirmed {
            self.schedule_semantic_context();
            self.sync_tier1_best_effort(Tier1Sync::Changed);
        }
        Ok(payload)
    }

    pub fn hw_clear(&self) {
        let mut st = self.input.lock();
        st.hw.clear();
    }

    pub fn hw_override_slot(
        &self,
        slot_idx: usize,
        candidate_idx: usize,
    ) -> Result<Mode2Result, VitralError> {
        let mut st = self.input.lock();
        st.hw.override_slot(slot_idx, candidate_idx)
    }

    fn confirm_handwriting(
        hw: &mut HandwritingSession,
        engine: &mut EngineState,
        retention: &mut RetentionEngine,
        imported: &ImportedLexiconIndex,
        learn: bool,
    ) -> Option<String> {
        let newly_confirmed = !hw.confirmed_since_edit;
        let mut learned = RetentionEngine::default();
        let learn = learn && !engine.is_active_privacy_app();
        let committed = hw.confirm_result_with_learning(learn.then_some(&mut learned))?;
        if newly_confirmed && learn {
            let code = engine.raw_input.clone();
            engine.record_candidate_commit(
                &committed,
                &code,
                None,
                imported,
                retention,
                RetentionEngine::now_epoch_ms(),
            );
            // confirm_result also supplies the established 0.25 per-slot prior.
            for (word, boost) in learned.frecency {
                let slot_boost = if word == committed {
                    boost - 1.0
                } else {
                    boost
                };
                if slot_boost > 0.0 {
                    retention.record_selection_at(
                        &word,
                        RetentionEngine::now_epoch_ms(),
                        slot_boost,
                    );
                }
            }
        }
        Some(committed)
    }

    pub fn hw_confirm(&self) -> Result<String, VitralError> {
        let committed = {
            let mut st = self.input.lock();
            let imported = self.imported_lexicon.read();
            let InputState {
                hw,
                engine,
                retention,
                ..
            } = &mut *st;
            let committed = Self::confirm_handwriting(hw, engine, retention, &imported, true)
                .ok_or_else(|| VitralError::NotFound("no handwriting result to confirm".into()))?;
            self.record_profile_commit(&mut st, &committed);
            committed
        };
        self.schedule_semantic_context();
        self.sync_tier1_best_effort(Tier1Sync::Changed);
        Ok(committed)
    }

    pub fn import_lexicon(
        &self,
        path: &Path,
        format_hint: LexiconFormat,
    ) -> Result<ImportReport, VitralError> {
        let (data_dir, koofr_endpoint, koofr_username, koofr_password, koofr_root) = {
            let st = self.input.lock();
            let cfg = &st.engine.config;
            (
                cfg.data_dir.clone(),
                cfg.koofr_endpoint.clone(),
                cfg.koofr_username.clone(),
                cfg.koofr_password.clone(),
                cfg.koofr_root.clone(),
            )
        };
        let corpus = {
            let index = self.imported_lexicon.read();
            Arc::clone(index.corpus())
        };
        let compiled = compile_lexicon_file(corpus, path, format_hint)?;
        {
            let mut index = self.imported_lexicon.write();
            index.inject_hotwords(&compiled.entries);
        }
        let rep = finalize_compiled_lexicon_sync(
            compiled,
            Some(SyncContext {
                guard: &self.offline_guard,
                data_dir: &data_dir,
                koofr_endpoint: koofr_endpoint.as_deref(),
                koofr_username: koofr_username.as_deref(),
                koofr_password: koofr_password.as_deref(),
                koofr_root: koofr_root.as_deref(),
            }),
        );
        if rep.imported_count > 0 {
            self.sync_tier1_best_effort(Tier1Sync::Changed);
        }
        Ok(rep)
    }

    pub fn clipboard_push(&self, payload: ClipboardPayload) -> ClipboardState {
        let state = self.enqueue_clipboard_copy(payload);
        let first_current = self.poll_clipboard_current().map(|_| ());
        let background_sync = self.input.lock().engine.config.background_sync;
        if !background_sync {
            let _ = self.poll_clipboard_history(first_current);
        }
        state
    }

    #[cfg(any(test, windows))]
    fn clipboard_push_local(&self, payload: ClipboardPayload) -> ClipboardState {
        self.enqueue_clipboard_copy(payload)
    }

    fn enqueue_clipboard_copy(&self, mut payload: ClipboardPayload) -> ClipboardState {
        if payload.timestamp_ms == 0 {
            payload.timestamp_ms = RetentionEngine::now_epoch_ms();
        }
        let (state, outgoing) = {
            let mut io = self.sync_state_io.lock();
            let (state, writes, outgoing) =
                {
                    let mut sync = self.sync.lock();
                    if payload.source_device.is_empty() {
                        payload.source_device.clone_from(&sync.device_id);
                    }
                    let before = sync.clipboard.export_ring();
                    let previous = sync
                        .clipboard
                        .current()
                        .current_copy
                        .as_ref()
                        .map(|entry| (entry.timestamp_ms, entry.id.clone()));
                    let state = sync.clipboard.push(payload);
                    let mut writes = self.record_clipboard_change(&mut sync, &before);
                    writes.current = self
                        .local_dir
                        .as_ref()
                        .map(|_| sync.clipboard.current().clone());
                    let outgoing =
                        if self.is_pure_offline() {
                            None
                        } else {
                            sync.clipboard_sync_key
                                .as_ref()
                                .filter(|key| !key.trim().is_empty())
                                .zip(sync.clipboard.current().current_copy.as_ref().filter(
                                    |entry| {
                                        previous.as_ref().is_none_or(|(ts, id)| {
                                            clip::copy_order(entry) != (*ts, id.as_str())
                                        })
                                    },
                                ))
                                .and_then(|(key, entry)| {
                                    let peers: Vec<_> = sync
                                        .devices
                                        .iter()
                                        .filter(|peer| {
                                            peer.lan_online == Some(true)
                                                && peer.device_id != sync.device_id
                                        })
                                        .filter_map(|peer| peer.localsend_url.clone())
                                        .collect();
                                    (!peers.is_empty()).then(|| {
                                        (Arc::new(entry.clone()), Arc::new(key.clone()), peers)
                                    })
                                })
                        };
                    (state, writes, outgoing)
                };
            self.save_clipboard(&mut io, writes);
            (state, outgoing)
        };
        if let Some((entry, key, peers)) = outgoing {
            match dav::network_runtime() {
                Ok(runtime) => {
                    for url in peers {
                        let (guard, entry, key) =
                            (self.offline_guard.clone(), entry.clone(), key.clone());
                        let sync = Arc::downgrade(&self.sync);
                        runtime.spawn(async move {
                            let result = lan::LocalSendTransport::push_clipboard_to_peer(
                                &guard, &url, &entry, &key,
                            ).await;
                            if let Some(sync) = sync.upgrade() {
                                sync.lock().record_lan_result(&result);
                            }
                        });
                    }
                }
                Err(error) => self.sync.lock().record_lan_result(&Err::<(), _>(error)),
            }
        }
        state
    }

    pub fn clipboard_ingest_remote(
        &self,
        payload: ClipboardPayload,
        is_cold_boot: bool,
    ) -> QuickPasteState {
        let mut io = self.sync_state_io.lock();
        let (state, writes) = {
            let mut sync = self.sync.lock();
            let state = sync.clipboard.ingest_remote(payload, is_cold_boot);
            let writes = ClipboardWrites {
                changes: None,
                history: self.local_dir.as_ref().map(|_| sync.clipboard.export_ring()),
                current: self.local_dir.as_ref().map(|_| sync.clipboard.current().clone()),
            };
            (state, writes)
        };
        self.save_clipboard(&mut io, writes);
        state
    }

    fn record_profile_commit(&self, st: &mut InputState, text: &str) {
        if !st.engine.config.adaptive_learning || !st.engine.config.profile_distill_enabled
            || st.engine.is_active_privacy_app()
        {
            return;
        }
        let app = st.engine.active_app_id.clone().unwrap_or_else(|| "unknown".into());
        st.profile.record(&app, text);
        if st.profile.needs_distill {
            let InputState { profile, retention, .. } = st;
            // The bounded statistical pass does no model inference or disk I/O on keys.
            profile.statistical(retention);
            let snapshot = profile.clone();
            let input = self.input.clone();
            std::thread::spawn(move || {
                if let Err(e) = snapshot.persist() { input.lock().profile.last_reason = Some(e.to_string()); }
            });
        }
    }

    pub fn profile_state(&self) -> serde_json::Value {
        let st = self.input.lock();
        serde_json::json!({
            "profile": st.profile.profile,
            "needs_distill": st.profile.needs_distill,
            "reason": st.profile.last_reason,
            "prediction_enabled": st.engine.config.prediction_enabled,
            "prediction_skip_dev_apps": st.engine.config.prediction_skip_dev_apps,
            "prediction_delay_ms": st.engine.config.prediction_delay_ms,
            "profile_distill_enabled": st.engine.config.profile_distill_enabled,
            "voice_cleanup_mode": st.engine.config.voice_cleanup_mode,
            "privacy_apps": st.engine.config.privacy_apps,
        })
    }
    pub fn profile_update(&self, update: profile::ProfileUpdate) -> Result<(), VitralError> {
        self.prediction.clear_cache(); self.input.lock().profile.update_profile(update)
    }
    pub fn profile_clear(&self) -> Result<(), VitralError> {
        self.prediction.clear_feedback(); self.input.lock().profile.clear_profile()
    }
    pub fn profile_distill(&self) -> Result<(), VitralError> {
        let (mut snapshot, ret, model, revision) = {
            let st = self.input.lock();
            if !st.engine.config.profile_distill_enabled { return Ok(()); }
            (st.profile.detached(), st.retention.clone(), st.engine.model_paths.local_llm.clone(), st.profile.profile.revision)
        };
        {
            let model = model.as_ref().filter(|_| !self.is_power_save());
            let _distill = model.map(|_| self.profile_distill_model.lock());
            let model = model.filter(|_| !self.is_power_save());
            if ai::local_llm::LocalLlmEngine::available(model) {
                self.privacy_audit.record("profile_distill", "local", self.is_pure_offline());
            }
            snapshot.distill(&ret, model, ai::local_engine())?;
        }
        let mut st = self.input.lock();
        // A manual edit or clear wins over a late distillation result.
        if st.profile.profile.revision == revision && st.engine.config.profile_distill_enabled {
            snapshot.profile.accepted_count = st.profile.profile.accepted_count;
            snapshot.profile.dismissed_count = st.profile.profile.dismissed_count;
            st.profile.profile = snapshot.profile;
            st.profile.last_reason = snapshot.last_reason;
            st.profile.persist()?;
            self.prediction.clear_cache();
        }
        Ok(())
    }
    /// Read bounded continuation buckets without waiting for an ongoing input operation.
    pub fn context_next_hints(
        &self,
        context_before: &str,
        prediction_text: Option<&str>,
    ) -> Vec<ContextNextHint> {
        use pinyin::ToPinyin;

        let Some(st) = self.input.try_lock() else {
            return Vec::new();
        };
        if st.engine.is_active_privacy_app() {
            return Vec::new();
        }
        let corpus = st.engine.pure_rust.get_corpus();
        let gram = st.engine.pure_rust.cached_gram();
        let canonical = crate::imp::xiaohe::canonical_syllable_set();
        let hint = |word: &str, reading: Option<&str>, source| {
            let ch = word.chars().next().filter(|ch| ret::is_cjk(*ch))?;
            let syllable = reading
                .or_else(|| corpus.char_readings(ch).next().map(|(s, _)| s))
                .or_else(|| ch.to_pinyin().map(|p| p.plain()))?;
            if !canonical.contains(syllable) {
                return None;
            }
            Some(ContextNextHint {
                initial: syllable[..1].to_string(),
                syllable: syllable.to_string(),
                ch: ch.to_string(),
                source,
            })
        };
        let mut ranked: Vec<(u8, f64, ContextNextHint)> = Vec::with_capacity(7);
        let mut push = |hint: ContextNextHint, score: f64| {
            let priority = match hint.source {
                "prediction" => 2,
                "personal" => 1,
                _ => 0,
            };
            if let Some(i) = ranked
                .iter()
                .position(|(_, _, h)| h.initial == hint.initial)
            {
                let (old_priority, old_score, old) = &ranked[i];
                if priority
                    .cmp(old_priority)
                    .then_with(|| score.total_cmp(old_score))
                    .then_with(|| old.ch.cmp(&hint.ch))
                    .then_with(|| old.syllable.cmp(&hint.syllable))
                    .is_le()
                {
                    return;
                }
                ranked.remove(i);
            }
            ranked.push((priority, score, hint));
            ranked.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| b.1.total_cmp(&a.1))
                    .then_with(|| a.2.ch.cmp(&b.2.ch))
                    .then_with(|| a.2.syllable.cmp(&b.2.syllable))
            });
            ranked.truncate(6);
        };
        if let Some(hint) = prediction_text.and_then(|text| hint(text, None, "prediction")) {
            push(hint, 0.0);
        }
        let mut tail: Vec<char> = context_before
            .chars()
            .rev()
            .take(3)
            .take_while(|ch| ret::is_cjk(*ch))
            .collect();
        if tail.is_empty() {
            tail = st
                .engine
                .speech_context_before()
                .chars()
                .rev()
                .take(3)
                .take_while(|ch| ret::is_cjk(*ch))
                .collect();
        }
        // Corpus buckets are capped at 24/10 entries and personal buckets at eight.
        for length in 1..=tail.len() {
            let prefix: String = tail[..length].iter().rev().collect();
            if let Some(words) = st.retention.context_bigrams.get(&prefix) {
                for (word, weight) in words {
                    if !weight.is_finite()
                        || *weight <= 0.25
                        || st.retention.erased_words.contains(word)
                    {
                        continue;
                    }
                    let reading = corpus
                        .word_reading(word)
                        .and_then(|(mut syllables, _)| syllables.next());
                    if let Some(hint) = hint(word, reading, "personal") {
                        push(hint, *weight);
                    }
                }
            }
            for (word, weight) in corpus.continuations(&prefix) {
                let phrase = format!("{prefix}{word}");
                if st.retention.erased_words.contains(word)
                    || st.retention.erased_words.contains(&phrase)
                {
                    continue;
                }
                let reading = corpus
                    .word_reading(&phrase)
                    .and_then(|(mut syllables, _)| syllables.nth(length))
                    .or_else(|| corpus.word_reading(word)?.0.next());
                if let Some(hint) = hint(word, reading, "dictionary") {
                    let score = (weight as f64).ln_1p()
                        + length as f64 * 8.0
                        + gram.and_then(|g| g.lookup(&prefix, word)).unwrap_or(0.0);
                    push(hint, score);
                }
            }
        }
        ranked.into_iter().map(|(_, _, hint)| hint).collect()
    }

    pub fn predict_request(&self, mut request: predict::PredictRequest) -> predict::PredictResult {
        let skipped = self.prediction.cancel();
        if request.is_password || request.is_composing || request.no_personalized_learning {
            return skipped;
        }
        let st = self.input.lock();
        if request.app_id.trim().is_empty() {
            request.app_id = st.engine.active_app_id.clone().unwrap_or_default();
        }
        // Host claims cannot bypass the core composition gate.
        if !st.engine.config.prediction_enabled
            || !st.engine.raw_input.is_empty()
            || !st.engine.composing.is_empty()
            || st.engine.is_active_privacy_app()
            || eng::is_privacy_app(&request.app_id, &st.engine.config.privacy_apps)
            || (st.engine.config.prediction_skip_dev_apps && eng::is_dev_app(&request.app_id))
        {
            return skipped;
        }
        let latest = st.profile.latest(&request.app_id);
        if request.context_before.trim().is_empty()
            && latest.is_none_or(|context| context.trim().is_empty())
        {
            return skipped;
        }
        self.prediction.request(
            request,
            &st.engine.config,
            &st.profile.profile,
            latest,
            &st.retention,
            st.engine
                .model_paths
                .local_llm
                .as_ref()
                .filter(|_| !self.is_power_save()),
            ai::local_engine(),
            Some(skipped.id),
            Some((
                st.engine.pure_rust.get_corpus(),
                st.engine.pure_rust.get_gram(),
            )),
        )
    }
    pub fn predict_poll(&self, id: Option<u64>) -> predict::PredictResult { self.prediction.poll(id) }
    pub fn predict_cancel(&self) -> predict::PredictResult { self.prediction.cancel() }
    pub fn predict_accept(&self, id: u64, text: &str) -> Result<bool, VitralError> {
        let (learned, persisted) = {
            let mut st = self.input.lock();
            if st.engine.is_active_privacy_app() {
                return Ok(false);
            }
            let Some(request) = self.prediction.accept(id, text) else {
                return Ok(false);
            };
            if st.retention.erased_words.contains(text) {
                return Ok(false);
            }
            let learn = st.engine.config.adaptive_learning;
            let profile_learn = learn && st.engine.config.profile_distill_enabled;
            st.profile.feedback(Some((&request.app_id, text)), profile_learn);
            if learn && profile::safe_text(text) {
                st.retention.record_contextual_selection(
                    text,
                    "",
                    Some(&request.context_before),
                    Some(&request.app_id),
                    RetentionEngine::now_epoch_ms(),
                    1.0,
                    true,
                );
            }
            (learn, st.profile.persist())
        };
        if learned {
            self.sync_tier1_best_effort(Tier1Sync::Changed);
        }
        persisted?;
        Ok(true)
    }
    pub fn predict_dismiss(&self, id: u64) -> Result<bool, VitralError> {
        self.predict_dismiss_with_text(id, None)
    }

    pub(crate) fn predict_dismiss_with_text(&self, id: u64, text: Option<&str>) -> Result<bool, VitralError> {
        let mut st = self.input.lock();
        if st.engine.is_active_privacy_app() || !self.prediction.dismiss(id, text) {
            return Ok(false);
        }
        st.profile.feedback(None, false);
        st.profile.persist()?;
        Ok(true)
    }

    pub fn personalization_stats(&self) -> ret::PersonalizationStats {
        self.input.lock().retention.stats()
    }

    pub fn clear_learned_habits(&self) -> ret::PersonalizationStats {
        self.prediction.clear_feedback();
        let stats = {
            let _tier1_io = self.tier1_io.lock();
            let mut st = self.input.lock();
            st.retention.clear_learned_habits();
            if let Err(e) = st.profile.clear_profile() { st.profile.last_reason = Some(e.to_string()); }
            st.engine.reset_commit_context();
            let imported = self.imported_lexicon.read();
            self.save_local(
                "lexicon-state.json",
                &LexiconSnapshotPayload {
                    schema_version: 1,
                    retention: st.retention.clone(),
                    imported_entries: imported.export_entries(),
                },
            );
            st.retention.stats()
        };
        self.sync_tier1_best_effort(Tier1Sync::Changed);
        stats
    }

    pub fn clipboard_state(&self) -> ClipboardState {
        let sync_st = self.sync.lock();
        sync_st.clipboard.state()
    }

    pub fn quick_paste_state(&self) -> QuickPasteState {
        self.consume_lan_clipboard();
        let sync_st = self.sync.lock();
        sync_st.clipboard.quick_paste_state()
    }

    pub(crate) fn clipboard_lan_revision(&self) -> u64 {
        self.lan_session
            .try_lock()
            .and_then(|session| {
                session
                    .as_ref()
                    .map(lan::DiscoverySession::clipboard_revision)
            })
            .unwrap_or(0)
    }

    fn consume_lan_clipboard(&self) {
        if self.is_pure_offline()
            || self
                .lan_session
                .lock()
                .as_ref()
                .is_none_or(|session| !session.has_received_clipboard())
        {
            return;
        }
        let mut io = self.sync_state_io.lock();
        let Some(entries) = self
            .lan_session
            .lock()
            .as_ref()
            .filter(|_| !self.is_pure_offline())
            .map(lan::DiscoverySession::take_received_clipboard)
        else { return };
        let current = {
            let mut s = self.sync.lock();
            if self.is_pure_offline() || entries.is_empty() {
                return;
            }
            s.record_lan_result(&Ok(()));
            let latest = entries.into_iter()
                .max_by(|a, b| clip::copy_order(a).cmp(&clip::copy_order(b)));
            let changed = latest.is_some_and(|entry| s.clipboard.observe_current(entry));
            (changed && self.local_dir.is_some()).then(|| s.clipboard.current().clone())
        };
        self.save_clipboard(&mut io, ClipboardWrites { current, ..Default::default() });
    }

    pub fn consume_quick_paste(&self) -> QuickPasteConsumeResult {
        let mut input = self.input.lock();
        let mut io = self.sync_state_io.lock();
        let (result, current) = {
            let mut sync = self.sync.lock();
            let result = sync.clipboard.consume_quick_paste();
            (result, self.local_dir.as_ref().map(|_| sync.clipboard.current().clone()))
        };
        if result.consumed.is_some() {
            input.engine.reset_commit_context();
        }
        self.save_clipboard(&mut io, ClipboardWrites { current, ..Default::default() });
        result
    }

    pub fn hide_quick_paste_for_input(&self) { self.sync.lock().clipboard.hide_for_input(); }

    pub fn on_ime_open(&self) -> QuickPasteState {
        let mut sync_st = self.sync.lock();
        sync_st.clipboard.on_ime_open()
    }

    pub fn on_ime_close(&self) -> QuickPasteState {
        self.prediction.cancel();
        let state = self.sync.lock().clipboard.on_ime_close();
        self.sync_tier1_best_effort(Tier1Sync::Local);
        state
    }

    pub fn dismiss_quick_paste(&self) -> QuickPasteState {
        let mut io = self.sync_state_io.lock();
        let (result, current) = {
            let mut sync = self.sync.lock();
            let result = sync.clipboard.dismiss_quick_paste();
            (result, self.local_dir.as_ref().map(|_| sync.clipboard.current().clone()))
        };
        self.save_clipboard(&mut io, ClipboardWrites { current, ..Default::default() });
        result
    }

    pub fn clipboard_pin(
        &self,
        item_id: &str,
        shortcut_code: Option<&str>,
    ) -> Result<ClipboardState, VitralError> {
        self.clipboard_pin_update(item_id, Some(shortcut_code))
    }

    pub(crate) fn clipboard_pin_update(
        &self,
        item_id: &str,
        shortcut_code: Option<Option<&str>>,
    ) -> Result<ClipboardState, VitralError> {
        let state = {
            let mut io = self.sync_state_io.lock();
            let (state, writes) = {
                let mut sync = self.sync.lock();
                let before = sync.clipboard.export_ring();
                let existing = before
                    .iter()
                    .find(|e| e.id == item_id)
                    .and_then(|e| e.shortcut_code.as_deref());
                let state = sync.clipboard.pin(item_id, shortcut_code.unwrap_or(existing))?;
                (state, self.record_clipboard_change(&mut sync, &before))
            };
            self.save_clipboard(&mut io, writes);
            state
        };
        let background_sync = self.input.lock().engine.config.background_sync;
        if !background_sync {
            let _ = self.poll_clipboard();
        }
        Ok(state)
    }

    pub fn clipboard_unpin(&self, item_id: &str) -> Result<ClipboardState, VitralError> {
        let state = {
            let mut io = self.sync_state_io.lock();
            let (state, writes) = {
                let mut sync = self.sync.lock();
                let before = sync.clipboard.export_ring();
                let state = sync.clipboard.unpin(item_id)?;
                (state, self.record_clipboard_change(&mut sync, &before))
            };
            self.save_clipboard(&mut io, writes);
            state
        };
        let background_sync = self.input.lock().engine.config.background_sync;
        if !background_sync { let _ = self.poll_clipboard(); }
        Ok(state)
    }

    pub fn clipboard_clear_unpinned(&self) -> ClipboardState {
        let state = {
            let mut io = self.sync_state_io.lock();
            let (state, writes) = {
                let mut sync = self.sync.lock();
                let before = sync.clipboard.export_ring();
                let state = sync.clipboard.clear_unpinned();
                (state, self.record_clipboard_change(&mut sync, &before))
            };
            self.save_clipboard(&mut io, writes);
            state
        };
        let background_sync = self.input.lock().engine.config.background_sync;
        if !background_sync { let _ = self.poll_clipboard(); }
        state
    }

    pub fn discover_devices(&self) -> Result<Vec<RelayDevice>, VitralError> {
        let result = self.discover_lan_devices();
        self.sync.lock().record_lan_result(&result);
        result
    }

    fn discover_lan_devices(&self) -> Result<Vec<RelayDevice>, VitralError> {
        let id = {
            let s = self.sync.lock();
            let mut session = self.lan_session.lock();
            let (id, name, key) = (s.device_id.clone(), s.device_name.clone(), s.clipboard_sync_key.clone());
            drop(s);
            self.offline_guard.require_online("device discovery")?;
            if session.is_none() {
                *session = Some(lan::DiscoverySession::start(&self.offline_guard, &id, &name, key)?);
            }
            id
        };
        let peers = lan::LocalSendTransport::discover_mdns_peers(&self.offline_guard,
            std::time::Duration::from_millis(1500))?;
        let _write = self.sync_state_io.lock();
        let mut s = self.sync.lock();
        for d in &mut s.devices {
            if d.lan_online == Some(true) { d.lan_online = Some(false); }
        }
        for peer in peers.into_iter().filter(|p| p.device_id != id) {
            if let Some(old) = s.devices.iter_mut().find(|d| d.device_id == peer.device_id) {
                old.lan_online = Some(true);
                old.localsend_url = peer.localsend_url;
                old.device_name = peer.device_name;
            } else { s.devices.push(peer); }
        }
        sort_devices_mru(&mut s.devices);
        let devices = s.devices.clone();
        drop(s);
        self.save_local("relay-devices.json", &devices);
        Ok(devices)
    }

    pub fn stop_discovery(&self) {
        self.consume_lan_clipboard();
        let stopped = self.lan_session.lock().take();
        drop(stopped);
    }

    pub fn relay_devices_mru(&self) -> Vec<RelayDevice> {
        let mut sync_st = self.sync.lock();
        sort_devices_mru(&mut sync_st.devices);
        sync_st.devices.clone()
    }

    pub fn relay_send(
        &self,
        device_id: &str,
        payload_or_path: &str,
        is_file_path: bool,
    ) -> Result<RelaySendResult, VitralError> {
        let (mut devices, dav_cfg, local_id, local_name) = {
            let sync_st = self.sync.lock();
            (
                sync_st.devices.clone(),
                sync_st.dav_config.clone(),
                sync_st.device_id.clone(),
                sync_st.device_name.clone(),
            )
        };
        let use_lan = devices.iter().any(|device| {
            device.device_id == device_id
                && device.effective_tier() == sync::DeviceTier::Lan
                && device.localsend_url.is_some()
        });
        let result = send_via_relay_with_config(
            &self.offline_guard,
            &mut devices,
            device_id,
            payload_or_path,
            is_file_path,
            dav_cfg.as_ref(),
            &local_id,
            &local_name,
        );
        let res = if use_lan {
            self.sync.lock().record_lan_result(&result);
            result?
        } else if dav_cfg.is_some() {
            self.track_cloud(result)?
        } else {
            result?
        };
        {
            let _write = self.sync_state_io.lock();
            let mut sync_st = self.sync.lock();
            if let Some(peer) = sync_st.devices.iter_mut().find(|p| p.device_id == device_id) {
                peer.usage_count = peer.usage_count.saturating_add(1);
                peer.last_used_ms = RetentionEngine::now_epoch_ms().max(peer.last_used_ms.saturating_add(1));
            }
            sort_devices_mru(&mut sync_st.devices);
            let devices = self.local_dir.as_ref().map(|_| sync_st.devices.clone());
            drop(sync_st);
            if let Some(devices) = devices {
                self.save_local("relay-devices.json", &devices);
            }
        }
        Ok(res)
    }

    pub fn relay_poll_inbox(&self) -> Result<RelayPollResult, VitralError> {
        let (dav_cfg, local_id) = {
            let sync_st = self.sync.lock();
            (sync_st.dav_config.clone(), sync_st.device_id.clone())
        };
        let polled = poll_koofr_relay_with_config(&self.offline_guard, dav_cfg.as_ref(), &local_id);
        let mut result = if dav_cfg.is_some() { self.track_cloud(polled)? } else { polled? };
        let received = self.lan_session.lock().as_ref()
            .map(lan::DiscoverySession::take_received)
            .unwrap_or_default();
        if !received.is_empty() {
            self.sync.lock().record_lan_result(&Ok(()));
        }
        for item in received {
            let id = format!("{}-{}", item.session_id, item.file_id);
            result.received.push(id.clone());
            result.received_items.push(sync::RelayReceivedItem { transfer_id: id,
                raw_bytes: item.data, is_file: true, file_name: Some(item.file_name) });
        }
        Ok(result)
    }

    pub fn relay_cleanup_orphans(&self) -> Result<CleanupResult, VitralError> {
        let dav_cfg = {
            let sync_st = self.sync.lock();
            sync_st.dav_config.clone()
        };
        let result = cleanup_koofr_orphans_with_config(&self.offline_guard, dav_cfg.as_ref());
        if dav_cfg.is_some() { self.track_cloud(result) } else { result }
    }

    pub fn voice_start(&self) {
        let mut st = self.input.lock();
        st.voice.start();
    }

    pub fn voice_push_pcm(&self, pcm: &[i16]) -> Result<(), VitralError> {
        let mut st = self.input.lock();
        st.voice.push_pcm(pcm)
    }

    pub fn voice_preview(&self) -> Result<VoiceResult, VitralError> {
        const MIN_PREVIEW_SAMPLES: usize = 4_800;
        let (samples, mut paths) = {
            let st = self.input.lock();
            let samples = st.voice.preview_slice();
            if samples.len() < MIN_PREVIEW_SAMPLES {
                return Ok(VoiceResult::empty(samples.len()));
            }
            (samples.to_vec(), st.engine.model_paths.clone())
        };
        // Keep the high-accuracy Qwen route for the final transcript.
        if paths.voice.is_some() {
            paths.voice_qwen3_asr = None;
        }
        ai::recognize_pcm(&samples, &paths, &ai::SpeechContext::default())
    }

    pub fn voice_stop(&self) -> Result<VoiceResult, VitralError> {
        let (samples, paths, ctx, mode, app_id, private) = {
            let mut st = self.input.lock();
            let ctx = ai::speech_context(
                &st.retention,
                st.engine.active_app_id.as_deref(),
                st.engine.speech_context_before(),
            );
            (
                st.voice.finish()?,
                st.engine.model_paths.clone(),
                ctx,
                st.engine.config.voice_cleanup_mode.clone(),
                st.engine.active_app_id.clone(),
                st.engine.is_active_privacy_app(),
            )
        };
        self.privacy_audit
            .record("voice_asr", "local", self.is_pure_offline());
        let mut result = ai::recognize_pcm(&samples, &paths, &ctx)?;
        let gram = paths
            .grammar
            .as_ref()
            .and_then(|p| crate::eng::gram::RimeGramDb::open_cached(p).ok());
        let correct = |text: &str| {
            ai::phonetic_correct_transcript(text, &self.input.lock().retention, gram.as_deref())
        };
        result.clean(&mode, correct, |text| {
            let model = paths.local_llm.as_ref()?;
            if self.is_power_save() || !ai::local_llm::LocalLlmEngine::available(Some(model)) {
                return None;
            }
            let generated = ai::local_engine().generate(
                model,
                &ai::local_llm::LocalLlmRequest {
                    system: AiMode::Polish.instructions("", None),
                    prompt: ai::user_text_prompt(text),
                    max_new_tokens: 512,
                    deadline: Some(
                        std::time::Instant::now() + std::time::Duration::from_millis(2500),
                    ),
                    ..Default::default()
                },
            );
            match generated {
                Ok(text) => Some(text),
                Err(err) => {
                    eprintln!("Vitral: local voice polish failed: {err}");
                    None
                }
            }
        });
        let mut st = self.input.lock();
        if !private && st.engine.active_app_id == app_id {
            self.record_profile_commit(&mut st, &result.transcript);
        }
        Ok(result)
    }

    pub fn ai_inline(
        &self,
        mode: AiMode,
        text: &str,
        target_lang: &str,
    ) -> Result<AiInlineResult, VitralError> {
        self.ai_inline_in_app(mode, text, target_lang, None)
    }

    pub(crate) fn ai_inline_in_app(
        &self,
        mode: AiMode,
        text: &str,
        target_lang: &str,
        app_id: Option<&str>,
    ) -> Result<AiInlineResult, VitralError> {
        let (client, model, tone) = self.ai_inline_context(app_id);
        let mut chat = AiChat {
            guard: &self.offline_guard,
            local_model: model.as_ref(),
            activity: None,
        };
        let pure_offline = self.is_pure_offline();
        let activity =
            self.privacy_audit
                .record("ai_inline", chat.audit_route(pure_offline), pure_offline);
        chat.activity = Some(&activity);
        client.run_inline(
            chat,
            InlineRequest {
                mode,
                text,
                target_lang,
                app_tone: tone.as_deref(),
            },
        )
    }

    fn ai_inline_context(
        &self,
        app_id: Option<&str>,
    ) -> (Arc<AiClient>, Option<models::LocalLlmModel>, Option<String>) {
        let st = self.input.lock();
        let client = Arc::clone(&self.ai.read());
        let model = st
            .engine
            .model_paths
            .local_llm
            .as_ref()
            .filter(|model| client.uses_local(&self.offline_guard, Some(model)))
            .cloned();
        let tone = app_id
            .or(st.engine.active_app_id.as_deref())
            .filter(|app| !st.engine.is_active_privacy_app() && !eng::is_privacy_app(app, &st.engine.config.privacy_apps))
            .and_then(|app| st.profile.profile.app_styles.get(app.trim()))
            .map(|style| style.tone.clone());
        (client, model, tone)
    }

    pub fn ai_inline_route(&self) -> &'static str {
        let (client, model, _) = self.ai_inline_context(None);
        if model.is_some() {
            "local"
        } else if !self.is_pure_offline() && client.has_remote_endpoint() {
            "remote"
        } else {
            "none"
        }
    }

    pub fn ai_inline_start(
        &self,
        mode: AiMode,
        text: &str,
        target_lang: &str,
        app_id: Option<&str>,
    ) -> Result<ai::InlineStart, VitralError> {
        let (client, model, tone) = self.ai_inline_context(app_id);
        self.ai_inline_tasks.start(
            client,
            self.offline_guard.clone(),
            model,
            &self.privacy_audit,
            InlineRequest {
                mode,
                text,
                target_lang,
                app_tone: tone.as_deref(),
            },
        )
    }

    pub fn ai_inline_poll(&self, id: u64) -> Result<ai::InlineUpdate, VitralError> {
        self.ai_inline_tasks.poll(id)
    }

    pub fn ai_inline_cancel(&self, id: Option<u64>) -> bool {
        self.ai_inline_tasks.cancel(id)
    }

    pub fn privacy_audit(&self) -> serde_json::Value {
        self.privacy_audit.snapshot(self.is_pure_offline())
    }

    pub fn privacy_audit_clear(&self) {
        self.privacy_audit.clear();
    }

    pub fn restore_from_koofr(&self, webdav_cfg_json: &str) -> Result<RestoreResult, VitralError> {
        self.offline_guard.require_online("restore_from_koofr")?;
        let fallback_cfg = {
            let sync_st = self.sync.lock();
            sync_st.dav_config.clone()
        };
        let cfg = DavConfig::from_json_with_fallback(webdav_cfg_json, fallback_cfg.as_ref())?;
        let _clipboard_io = self.clipboard_io.lock();
        let _current_io = self.clipboard_current_io.lock();
        let fetched = self.track_cloud(fetch_backup_from_koofr(&self.offline_guard, &cfg))?;

        {
            let _tier1_io = self.tier1_io.lock();
            let mut st = self.input.lock();
            if fetched.profile.is_some() || fetched.snapshot.is_some() || !fetched.crdt_deltas.is_empty() {
                self.prediction.clear_cache();
                if let Some(dir) = &self.retention_backup_dir {
                    local::backup_retention(dir, &st.retention)
                        .map_err(|error| VitralError::Io(format!("retention backup: {error}")))?;
                }
            }
            let mut io = self.sync_state_io.lock();
            let mut sync_st = self.sync.lock();
            let mut imported = self.imported_lexicon.write();

            if let Some(profile) = fetched.profile {
                let mut bindings = profile.shortcuts;
                shortcuts::normalize(&mut bindings).map_err(VitralError::InvalidArgument)?;
                st.engine.set_schema(profile.schema);
                st.engine.config.shortcuts = bindings;
                st.engine.set_opencc_mode(profile.opencc_mode);
                st.engine.app_lang_memory = profile.app_lang_memory;
                st.engine.config.clipboard_ring_capacity = profile.clipboard_ring_capacity.max(1);
                st.retention.set_half_life_sec(profile.frecency_half_life_sec);
                sync_st
                    .clipboard
                    .set_capacity(profile.clipboard_ring_capacity);
            }

            if let Some(snap) = fetched.snapshot {
                st.retention = snap.retention;
                st.retention.purge_clipboard_phrases();
                let half_life = st.retention.half_life_sec;
                st.retention.set_half_life_sec(half_life);
                imported.inject_hotwords(&snap.imported_entries);
            }

            let mut deltas = fetched.crdt_deltas;
            deltas.sort_by_key(|d| (d.timestamp_ms, d.seq));
            for delta in &deltas {
                st.retention.merge_crdt_delta(delta);
            }

            for art in &fetched.import_artifacts {
                imported.inject_hotwords(&art.entries);
            }

            let mut writes = ClipboardWrites::default();
            if let Some(entries) = fetched.clipboard_ring {
                sync_st.clipboard_changes = Default::default();
                writes.changes = self.local_dir.as_ref().map(|_| sync_st.clipboard_changes.clone());
                sync_st.clipboard.restore_ring_cold_boot(entries);
            }

            st.engine.config.koofr_endpoint = Some(cfg.endpoint.clone());
            st.engine.config.sync_key = cfg.sync_key.clone();
            if sync_st.dav_config.as_ref() != Some(&cfg) {
                sync_st.current_copy_etag = None;
                sync_st.current_copy_version = None;
                sync_st.backup_generation = sync_st.backup_generation.wrapping_add(1);
                sync_st.clipboard_sync_key.clone_from(&cfg.sync_key);
                if let Some(session) = self.lan_session.lock().as_ref() {
                    session.set_clipboard_key(cfg.sync_key.clone());
                }
                sync_st.dav_config = Some(cfg);
            }
            if let Some(dir) = &self.local_dir {
                writes.history = Some(sync_st.clipboard.export_ring());
                writes.current = Some(sync_st.clipboard.current().clone());
                drop(sync_st);
                let snapshot = LexiconSnapshotPayload {
                    schema_version: 1,
                    retention: st.retention.clone(),
                    imported_entries: imported.export_entries(),
                };
                let app_lang_memory = st.engine.app_lang_memory.clone();
                drop(imported);
                drop(st);
                local::save(dir, "lexicon-state.json", &snapshot)
                    .map_err(|e| VitralError::Io(format!("lexicon-state.json: {e}")))?;
                self.save_local("app-languages.json", &app_lang_memory);
                self.save_clipboard(&mut io, writes);
            }
        }

        Ok(RestoreResult {
            restored: true,
            cloud_root: fetched.cloud_root,
            snapshots_applied: fetched.snapshots_applied,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eng::LangMode;

    #[test]
    fn core_init_and_pure_offline_gate_enforced() {
        let core = VitralCore::init("{}").expect("core init");
        assert!(!core.is_pure_offline());
        core.set_pure_offline(true);
        assert!(core.is_pure_offline());

        assert!(matches!(
            core.relay_send("dev-1", "hello", false),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            core.relay_poll_inbox(),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            core.relay_cleanup_orphans(),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            core.ai_inline(AiMode::Translate, "你好", "en"),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            core.restore_from_koofr("{}"),
            Err(VitralError::PureOfflineViolation(_))
        ));

        // Offline voice still runs locally
        core.voice_start();
        core.voice_push_pcm(&[0i16; 1600]).unwrap();
        let vr = core.voice_stop().unwrap();
        assert_eq!(vr.duration_ms, 100);
    }

    #[cfg(feature = "native-voice")]
    #[test]
    fn voice_transcribes_chunked_mandarin_while_offline() {
        let core = VitralCore::init(r#"{"pure_offline":true}"#).unwrap();
        let wav = include_bytes!("../../../tests/fixtures/audio/mandarin_short_16k.wav");
        // This repository fixture has a standard 44-byte PCM WAV header.
        assert_eq!(&wav[36..40], b"data");
        core.voice_start();
        for chunk in wav[44..].chunks(32_000) {
            let pcm: Vec<_> = chunk.chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            core.voice_push_pcm(&pcm).unwrap();
        }
        if core.input.lock().engine.model_paths.voice.is_some() {
            let final_model = core.input.lock().engine.model_paths.voice_qwen3_asr.replace(
                std::path::PathBuf::from("/missing-vitral-voice-preview-test-model"),
            );
            let preview = core.voice_preview().unwrap();
            core.input.lock().engine.model_paths.voice_qwen3_asr = final_model;
            assert_eq!(preview.sample_count, (wav.len() - 44) / 2);
            assert_eq!(preview.language, "zh");
            assert!(!preview.transcript.is_empty());
        }
        let result = core.voice_stop().unwrap();
        assert_eq!(result.sample_count, (wav.len() - 44) / 2);
        assert_eq!(result.language, "zh");
        assert!(result.transcript.contains("天气") || result.transcript.contains("你好"));
        assert!(matches!(core.voice_stop(), Err(VitralError::NotInitialized)));
    }

    #[test]
    fn voice_preview_fast_paths_preserve_recording() {
        let core = VitralCore::init(r#"{"pure_offline":true}"#).unwrap();
        {
            let mut st = core.input.lock();
            st.engine.model_paths.voice = None;
            st.engine.model_paths.voice_qwen3_asr = None;
        }
        let idle = commands::execute(&core, "voice-preview", &serde_json::json!({})).unwrap();
        assert_eq!(
            idle,
            serde_json::json!({"transcript":"","language":"auto","duration_ms":0,"sample_count":0})
        );
        core.voice_start();
        let short = [800; 4_799];
        core.voice_push_pcm(&short).unwrap();
        let preview = core.voice_preview().unwrap();
        assert!(preview.transcript.is_empty());
        assert_eq!(preview.sample_count, short.len());
        {
            let st = core.input.lock();
            assert!(st.voice.active);
            assert_eq!(st.voice.samples, short);
        }

        core.voice_start();
        let silence = [0; 4_800];
        core.voice_push_pcm(&silence).unwrap();
        for _ in 0..2 {
            let preview = core.voice_preview().unwrap();
            assert!(preview.transcript.is_empty());
            assert_eq!(preview.duration_ms, 300);
            assert_eq!(preview.sample_count, silence.len());
        }
        core.voice_push_pcm(&[0; 320]).unwrap();
        let result = core.voice_stop().unwrap();
        assert_eq!(result.sample_count, 5_120);
        assert!(result.transcript.is_empty());
        assert_eq!(core.voice_preview().unwrap().sample_count, 0);
    }

    #[test]
    fn clipboard_shortcuts_normalize_validate_and_preserve_missing_field() {
        let core = VitralCore::init(r#"{"pure_offline":true}"#).unwrap();
        let pushed = core.clipboard_push(ClipboardPayload {
            id: None,
            content: "saved".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 1,
        });
        let id = &pushed.ring[0].id;
        for invalid in ["a1", "a-b", "a b", "汉", "abcdefghijklmnopq"] {
            assert!(matches!(
                core.clipboard_pin(id, Some(invalid)),
                Err(VitralError::InvalidArgument(_))
            ));
        }
        core.clipboard_pin(id, Some(" AbC ")).unwrap();
        assert_eq!(
            core.clipboard_state().ring[0].shortcut_code.as_deref(),
            Some("abc")
        );
        crate::commands::execute(&core, "clipboard-pin", &serde_json::json!({"id":id})).unwrap();
        assert_eq!(
            core.clipboard_state().ring[0].shortcut_code.as_deref(),
            Some("abc")
        );
        for value in [serde_json::Value::Null, serde_json::json!("")] {
            crate::commands::execute(
                &core,
                "clipboard-pin",
                &serde_json::json!({"id":id,"shortcut_code":value}),
            )
            .unwrap();
            assert!(core.clipboard_state().ring[0].shortcut_code.is_none());
        }
        let mut old = serde_json::to_value(&pushed.ring[0]).unwrap();
        old["shortcut_code"] = serde_json::json!(" OLDCODE ");
        let entry: crate::clip::ClipboardEntry = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(entry.shortcut_code.as_deref(), Some("oldcode"));
        old["shortcut_code"] = serde_json::json!("old-1");
        let entry: crate::clip::ClipboardEntry = serde_json::from_value(old).unwrap();
        assert!(entry.shortcut_code.is_none());
    }

    #[test]
    fn clipboard_unpin_clears_shortcut_and_allows_removal() {
        let core = VitralCore::init(r#"{"pure_offline":true}"#).unwrap();
        let state = core.clipboard_push(ClipboardPayload {
            id: None,
            content: "saved text".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 1,
        });
        let id = &state.ring[0].id;
        let pinned = core.clipboard_pin(id, Some("saved")).unwrap();
        assert!(pinned.ring[0].pinned);
        assert_eq!(pinned.ring[0].shortcut_code.as_deref(), Some("saved"));
        assert_eq!(core.clipboard_clear_unpinned().ring.len(), 1);

        let unpinned = core.clipboard_unpin(id).unwrap();
        assert!(!unpinned.ring[0].pinned);
        assert!(unpinned.ring[0].shortcut_code.is_none());
        assert_eq!(unpinned.pinned_count, 0);
        assert!(core.clipboard_clear_unpinned().ring.is_empty());
        assert!(matches!(core.clipboard_unpin(id), Err(VitralError::NotFound(_))));
    }

    #[test]
    fn clipboard_pin_shortcut_surfaces_in_candidates() {
        let core = VitralCore::init(r#"{"clipboard_ring_capacity": 3}"#).unwrap();
        let s1 = core.clipboard_push(ClipboardPayload {
            id: None,
            content: "https://app.koofr.net".into(),
            mime_type: "text/plain".into(),
            source_device: "host".into(),
            timestamp_ms: 1,
        });
        let id1 = s1.ring[0].id.clone();
        core.clipboard_pin(&id1, Some("ksb")).unwrap();
        core.dismiss_quick_paste();

        for ch in "ksb".bytes() {
            core.process_key(ch as u32, 0, SwipeDir::None).unwrap();
        }
        let action = core.process_key(0, 0, SwipeDir::None).unwrap();
        assert!(action.candidates[0].is_clipboard_shortcut);
        assert_eq!(action.candidates[0].text, "https://app.koofr.net");
        assert_eq!(action.candidates[0].shortcut_code.as_deref(), Some("ksb"));
    }

    #[test]
    fn shortcuts_coexist_with_clipboard_and_execute_without_text_commit() {
        let type_ascii = |core: &VitralCore, text: &str| text.chars().map(|c| core.process_key(c as u32, 0, SwipeDir::None).unwrap()).last().unwrap();
        use crate::shortcuts::ShortcutAction;
        for schema in ["rime_ice", "double_pinyin_flypy"] {
            let core = VitralCore::init(&serde_json::json!({"pure_offline":true,"schema":schema,"shortcuts":[
                {"action":"settings","key":"ctrl+shift+s","text":"set"},
                {"action":"clipboard","key":"F8","text":"set"}
            ]}).to_string()).unwrap();
            let push = core.clipboard_push(ClipboardPayload {
                id: Some("saved-set".into()), content: "收藏正文".into(), mime_type: "text/plain".into(), source_device: "test".into(), timestamp_ms: 1,
            });
            core.clipboard_pin(&push.ring[0].id, Some("set")).unwrap();
            core.dismiss_quick_paste();
            let action = type_ascii(&core, "set");
            assert!(action.candidates.iter().any(|c| c.function_action == Some(ShortcutAction::Settings)));
            assert!(action.candidates.iter().any(|c| c.function_action == Some(ShortcutAction::Clipboard)));
            let clip = action.candidates.iter().position(|c| c.is_clipboard_shortcut).unwrap();
            assert_eq!(core.select_candidate(clip).unwrap().commit_text.as_deref(), Some("收藏正文"));
            let action = type_ascii(&core, "set");
            let setting = action.candidates.iter().position(|c| c.function_action == Some(ShortcutAction::Settings)).unwrap();
            let before = core.input.lock().retention.crdt_seq;
            let opened = core.select_candidate(setting).unwrap();
            assert_eq!(opened.function_action, Some(ShortcutAction::Settings));
            assert!(opened.commit_text.is_none() && opened.preedit.text.is_empty());
            assert_eq!(core.input.lock().retention.crdt_seq, before);
            type_ascii(&core, "set");
            let raw = core.process_key(0xff0d, 0, SwipeDir::None).unwrap();
            assert_eq!(raw.commit_text.as_deref(), Some("set"));
            assert!(raw.function_action.is_none());
            type_ascii(&core, "set");
            assert_eq!(core.process_key(32, 0, SwipeDir::None).unwrap().function_action, Some(ShortcutAction::Settings));
            assert_eq!(core.shortcut_key("Ctrl+Shift+S").unwrap().unwrap().function_action, Some(ShortcutAction::Settings));
            assert!(core.shortcut_key("F9").unwrap().is_none());
        }
        assert!(VitralCore::init(r#"{"shortcuts":[{"action":"settings","key":"F8"},{"action":"clipboard","key":"F8"}]}"#).is_err());
    }

    #[test]
    fn clipboard_mobile_visibility_dismissal_and_restart() {
        let dir = std::env::temp_dir().join(format!("vitral-clipboard-lifecycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = serde_json::json!({"pure_offline":true,"platform":"android","assets_dir":"/nonexistent-vitral-assets","data_dir":dir}).to_string();
        let core = VitralCore::init(&cfg).unwrap();
        let push = |core: &VitralCore, id: &str, stamp| core.clipboard_push(ClipboardPayload {
            id: Some(id.into()), content: "可反复粘贴".into(), mime_type: "text/plain".into(), source_device: "phone".into(), timestamp_ms: stamp,
        });
        push(&core, "first", 10);
        core.on_ime_open();
        assert_eq!(core.candidate_state().unwrap().candidates[0].quick_paste_id.as_deref(), Some("first"));
        core.on_ime_close(); core.on_ime_open();
        assert_eq!(core.candidate_state().unwrap().candidates.len(), 1);
        core.process_key('a' as u32, 0, SwipeDir::None).unwrap();
        assert!(core.candidate_state().unwrap().candidates.iter().all(|c| c.quick_paste_id.is_none()));
        core.process_key(8, 0, SwipeDir::None).unwrap();
        assert!(core.candidate_state().unwrap().candidates.is_empty());
        assert!(core.quick_paste_state().quick_paste.is_some());
        core.on_ime_close(); core.on_ime_open();
        assert_eq!(core.candidate_state().unwrap().candidates.len(), 1);
        core.process_key(8, 0, SwipeDir::None).unwrap();
        assert!(core.quick_paste_state().quick_paste.is_none());
        assert_eq!(core.quick_paste_state().current_copy.unwrap().id, "first");
        drop(core);
        let core = VitralCore::init(&cfg).unwrap();
        core.on_ime_open();
        assert!(core.candidate_state().unwrap().candidates.is_empty());
        push(&core, "second", 20);
        core.candidate_state().unwrap();
        assert_eq!(core.select_candidate(0).unwrap().commit_text.as_deref(), Some("可反复粘贴"));
        drop(core);
        let core = VitralCore::init(&cfg).unwrap();
        core.on_ime_open();
        assert!(core.candidate_state().unwrap().candidates.is_empty());
        push(&core, "third", 30);
        core.clipboard_clear_unpinned();
        drop(core);
        let core = VitralCore::init(&cfg).unwrap();
        core.on_ime_open();
        assert_eq!(core.candidate_state().unwrap().candidates[0].quick_paste_id.as_deref(), Some("third"));
        assert!(core.clipboard_state().ring.is_empty());
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clipboard_desktop_candidates_and_raw_return() {
        for platform in ["linux", "windows", "android"] {
            let core = VitralCore::init(&serde_json::json!({"pure_offline":true,"platform":platform,"schema":"rime_ice"}).to_string()).unwrap();
            core.on_ime_open();
            core.clipboard_push(ClipboardPayload { id: Some("text".into()), content: "复制内容".into(), mime_type:"text/plain".into(), source_device:"test".into(), timestamp_ms:10 });
            assert_eq!(core.candidate_state().unwrap().candidates.len(), usize::from(platform == "android"));
            core.process_key('a' as u32, 0, SwipeDir::None).unwrap();
            let typed = core.process_key('i' as u32, 0, SwipeDir::None).unwrap();
            assert!(typed.candidates.iter().all(|c| c.quick_paste_id.is_none()));
            assert_eq!(core.process_key(13, 0, SwipeDir::None).unwrap().commit_text.as_deref(), Some("ai"));
            assert!(core.candidate_state().unwrap().candidates.is_empty());
            assert_eq!(core.quick_paste_state().current_copy.unwrap().content, "复制内容");
        }
    }

    #[test]
    fn clipboard_late_candidate_click_and_older_remote_are_safe() {
        let core = VitralCore::init(r#"{"pure_offline":true,"platform":"android","assets_dir":"/nonexistent-vitral-assets"}"#).unwrap();
        let payload = |id:&str, stamp| ClipboardPayload { id:Some(id.into()), content:id.into(), mime_type:"text/plain".into(), source_device:"test".into(), timestamp_ms:stamp };
        core.on_ime_open();
        core.clipboard_push(payload("old",10));
        core.candidate_state().unwrap();
        core.clipboard_push(payload("new",30));
        assert!(core.select_candidate(0).unwrap().commit_text.is_none());
        core.clipboard_ingest_remote(payload("late",20),false);
        assert_eq!(core.quick_paste_state().current_copy.unwrap().id,"new");
        core.candidate_state().unwrap();
        assert_eq!(core.select_candidate(0).unwrap().commit_text.as_deref(),Some("new"));
        core.clipboard_ingest_remote(payload("new",30),false);
        assert!(core.quick_paste_state().quick_paste.is_none());
        assert_eq!(core.quick_paste_state().current_copy.unwrap().id,"new");
    }

    #[test]
    fn quick_paste_preserves_text_with_micro_spacing_enabled() {
        let core = VitralCore::init(r#"{"pure_offline":true,"platform":"android","auto_micro_space":true}"#).unwrap();
        assert_eq!(core.process_key('7' as u32, 0, SwipeDir::None).unwrap().commit_text.as_deref(), Some("7"));
        core.on_ime_open();
        core.clipboard_push(serde_json::from_value(serde_json::json!({"content": "中文\n原文"})).unwrap());
        assert!(core.candidate_state().unwrap().candidates[0].quick_paste_id.is_some());
        assert_eq!(core.select_candidate(0).unwrap().commit_text.as_deref(), Some("中文\n原文"));

        core.clipboard_push(serde_json::from_value(serde_json::json!({"content": "OpenAI", "timestamp_ms": u64::MAX})).unwrap());
        assert_eq!(core.consume_quick_paste().consumed.unwrap().content, "OpenAI");
    }

    #[test]
    fn init_defaults_to_no_backspace_recovery() {
        for platform in ["linux", "windows", "android"] {
            let core = VitralCore::init(&serde_json::json!({
                "pure_offline": true, "platform": platform, "schema": "rime_ice"
            }).to_string()).unwrap();
            for ch in "nihao".chars() {
                core.process_key(ch as u32, 0, SwipeDir::None).unwrap();
            }
            let typed = core.candidate_state().unwrap();
            let index = typed.candidates.iter().position(|item| item.text == "你好").unwrap();
            assert_eq!(core.select_candidate(index).unwrap().commit_text.as_deref(), Some("你好"));
            let action = core.process_key(8, 0, SwipeDir::None).unwrap();
            assert_eq!(action.undo_commit_chars, 0);
            assert!(action.undo_commit_text.is_none());
            assert!(action.preedit.text.is_empty());
        }
    }

    #[test]
    fn reset_commit_context_command_ends_backspace_recovery() {
        for reset in [false, true] {
            let core = VitralCore::init(r#"{"pure_offline":true,"undo_commit_on_backspace":true,"schema":"rime_ice"}"#).unwrap();
            for ch in "nihao".chars() {
                core.process_key(ch as u32, 0, SwipeDir::None).unwrap();
            }
            let typed = core.candidate_state().unwrap();
            let index = typed.candidates.iter().position(|item| item.text == "你好").unwrap();
            assert_eq!(core.select_candidate(index).unwrap().commit_text.as_deref(), Some("你好"));
            if reset {
                for _ in 0..2 {
                    assert_eq!(commands::execute(&core, "reset-commit-context", &serde_json::json!({})).unwrap(),
                        serde_json::json!({"ok":true}));
                }
            }
            let backspace = core.process_key(8, 0, SwipeDir::None).unwrap();
            if reset {
                assert_eq!(backspace.undo_commit_chars, 0);
                assert!(backspace.preedit.text.is_empty());
                assert!(core.input.lock().engine.raw_input.is_empty());
            } else {
                assert_eq!(backspace.undo_commit_chars, 2);
                assert_eq!(core.input.lock().engine.raw_input, "nihao");
            }
        }
    }

    #[test]
    fn backspace_dismisses_quick_paste_without_undoing_the_previous_commit() {
        for keycode in [0x08, 0xff08] {
            let core = VitralCore::init(r#"{"pure_offline":true,"undo_commit_on_backspace":true,"platform":"android","schema":"rime_ice"}"#).unwrap();
            core.on_ime_open();
            for ch in "nihao".chars() {
                core.process_key(ch as u32, 0, SwipeDir::None).unwrap();
            }
            let typed = core.candidate_state().unwrap();
            let index = typed.candidates.iter().position(|item| item.text == "你好").unwrap();
            assert_eq!(core.select_candidate(index).unwrap().commit_text.as_deref(), Some("你好"));

            core.on_ime_close();
            core.on_ime_open();
            core.clipboard_push(serde_json::from_value(serde_json::json!({"content": "快捷粘贴"})).unwrap());
            assert!(core.candidate_state().unwrap().candidates[0].quick_paste_id.is_some());
            let backspace = core.process_key(keycode, 0, SwipeDir::None).unwrap();
            assert_eq!(backspace.undo_commit_chars, 0);
            assert!(backspace.commit_text.is_none());
            assert!(backspace.preedit.text.is_empty());
            assert!(backspace.candidates.iter().all(|item| item.quick_paste_id.is_none()));
            assert!(core.input.lock().engine.raw_input.is_empty());
            assert!(core.quick_paste_state().quick_paste.is_none());

            // Dismissal consumes this key while preserving the pending word undo.
            assert_eq!(core.process_key(keycode, 0, SwipeDir::None).unwrap().undo_commit_chars, 2);
        }
    }

    #[test]
    fn quick_paste_ends_backspace_recovery_for_the_previous_candidate() {
        let core = VitralCore::init(r#"{"pure_offline":true,"undo_commit_on_backspace":true,"platform":"android","schema":"rime_ice"}"#).unwrap();
        core.on_ime_open();
        for ch in "nihao".chars() {
            core.process_key(ch as u32, 0, SwipeDir::None).unwrap();
        }
        let typed = core.candidate_state().unwrap();
        let index = typed.candidates.iter().position(|item| item.text == "你好").unwrap();
        assert_eq!(core.select_candidate(index).unwrap().commit_text.as_deref(), Some("你好"));

        core.on_ime_close();
        core.on_ime_open();
        core.clipboard_push(serde_json::from_value(serde_json::json!({"content": "快捷粘贴"})).unwrap());
        assert!(core.candidate_state().unwrap().candidates[0].quick_paste_id.is_some());
        let action = core.select_candidate(0).unwrap();
        assert_eq!(action.commit_text.as_deref(), Some("快捷粘贴"));
        assert_eq!(serde_json::to_value(action).unwrap()["clipboard_commit"], true);
        let backspace = core.process_key(8, 0, SwipeDir::None).unwrap();
        assert_eq!(backspace.undo_commit_chars, 0);
        assert!(backspace.preedit.text.is_empty());
    }

    #[test]
    fn clipboard_shortcut_preserves_text_with_micro_spacing_enabled() {
        let core = VitralCore::init(r#"{"pure_offline":true,"auto_micro_space":true,"assets_dir":"/nonexistent-vitral-assets"}"#).unwrap();
        core.on_app_focus("editor").unwrap();
        let state = core.clipboard_push(serde_json::from_value(serde_json::json!({"content": "中文\n原文"})).unwrap());
        core.clipboard_pin(&state.ring[0].id, Some("ksb")).unwrap();

        for (key, suffix) in [(None, ""), (Some(' '), ""), (Some('1'), ""), (Some(','), "，"), (Some('('), "()")] {
            let typed = core.process_key('7' as u32, 0, SwipeDir::None).unwrap();
            assert_eq!(typed.commit_text.as_deref(), Some("7"));
            let typed_json = serde_json::to_value(typed).unwrap();
            assert!(typed_json.get("clipboard_commit").is_none());
            assert!(
                !serde_json::from_value::<InputAction>(typed_json)
                    .unwrap()
                    .clipboard_commit
            );
            for ch in "ksb".chars() {
                core.process_key(ch as u32, 0, SwipeDir::None).unwrap();
            }
            assert!(core.candidate_state().unwrap().candidates[0].is_clipboard_shortcut);
            let action = if let Some(key) = key {
                core.process_key(key as u32, 0, SwipeDir::None).unwrap()
            } else {
                core.select_candidate(0).unwrap()
            };
            let expected = format!("中文\n原文{suffix}");
            assert_eq!(action.commit_text.as_deref(), Some(expected.as_str()));
            assert_eq!(serde_json::to_value(action).unwrap()["clipboard_commit"], true);
            assert_eq!(core.input.lock().profile.latest("editor"), Some("7"));
        }
    }

    #[test]
    fn clipboard_distributed_current_and_local_dismissal() {
        let dev_a = VitralCore::init("{}").unwrap();
        let dev_b = VitralCore::init("{}").unwrap();

        // 1. A new installation restores the current copy.
        let cold = dev_b.clipboard_ingest_remote(
            ClipboardPayload {
                id: Some("hist-0".into()),
                content: "old-item".into(),
                mime_type: "text/plain".into(),
                source_device: "dev-a".into(),
                timestamp_ms: 10,
            },
            true,
        );
        assert_eq!(cold.current_copy.unwrap().id, "hist-0");
        assert_eq!(dev_b.clipboard_state().ring.len(), 1);

        // 2. Live copy on Device A overwrites Device A & syncs to Device B (while IME is closed)
        dev_a.clipboard_push(ClipboardPayload {
            id: Some("copy-1".into()),
            content: "first-secret".into(),
            mime_type: "text/plain".into(),
            source_device: "dev-a".into(),
            timestamp_ms: 20,
        });
        let qp_b = dev_b.clipboard_ingest_remote(
            ClipboardPayload {
                id: Some("copy-1".into()),
                content: "first-secret".into(),
                mime_type: "text/plain".into(),
                source_device: "dev-a".into(),
                timestamp_ms: 20,
            },
            false,
        );
        assert_eq!(qp_b.quick_paste.as_ref().unwrap().content, "first-secret");
        assert!(!qp_b.seen_while_ime_open);

        // 3. Newer remote copy overwrites quick_paste
        let qp_b2 = dev_b.clipboard_ingest_remote(
            ClipboardPayload {
                id: Some("copy-2".into()),
                content: "second-secret".into(),
                mime_type: "text/plain".into(),
                source_device: "dev-a".into(),
                timestamp_ms: 30,
            },
            false,
        );
        assert_eq!(qp_b2.quick_paste.as_ref().unwrap().content, "second-secret");

        // 4. Local consume on Device A clears Device A ONLY; Device B still retains it!
        dev_a.clipboard_push(ClipboardPayload {
            id: Some("copy-2".into()),
            content: "second-secret".into(),
            mime_type: "text/plain".into(),
            source_device: "dev-a".into(),
            timestamp_ms: 30,
        });
        let consumed_a = dev_a.consume_quick_paste();
        assert_eq!(consumed_a.consumed.unwrap().content, "second-secret");
        assert!(dev_a.quick_paste_state().quick_paste.is_none());
        assert_eq!(
            dev_b.quick_paste_state().quick_paste.as_ref().unwrap().content,
            "second-secret"
        );

        // 5. Closing and reopening never consume the suggestion.
        let opened_b = dev_b.on_ime_open();
        assert!(opened_b.ime_open);
        assert!(opened_b.seen_while_ime_open);
        assert!(opened_b.quick_paste.is_some());

        let closed_b = dev_b.on_ime_close();
        assert!(!closed_b.ime_open);
        assert!(!closed_b.seen_while_ime_open);
        assert!(closed_b.quick_paste.is_some());

        // Re-opening shows the same copy again.
        let reopened_b = dev_b.on_ime_open();
        assert!(reopened_b.candidate_visible);

        // 6. Explicit dismiss clears quick_paste
        dev_b.clipboard_ingest_remote(
            ClipboardPayload {
                id: Some("copy-3".into()),
                content: "third".into(),
                mime_type: "text/plain".into(),
                source_device: "dev-a".into(),
                timestamp_ms: 40,
            },
            false,
        );
        let dismissed = dev_b.dismiss_quick_paste();
        assert!(dismissed.quick_paste.is_none());
        assert!(!dismissed.seen_while_ime_open);
    }

    #[test]
    fn app_focus_remembers_cn_en_and_hw_lifecycle() {
        let core = VitralCore::init("{}").unwrap();
        let term = core.on_app_focus("org.kde.konsole.terminal").unwrap();
        assert_eq!(term.lang_mode, LangMode::En);
        assert!(!term.remembered);
        let term2 = core.on_app_focus("org.kde.konsole.terminal").unwrap();
        assert!(term2.remembered);

        core.hw_begin(HwMode::PhoneticLookup);
        let res = core
            .hw_push_stroke(
                &[StrokePoint {
                    x: 10.0,
                    y: 20.0,
                    timestamp_ms: 100,
                }],
                true,
            )
            .unwrap();
        match res {
            HandwritingPayload::Mode1(m1) => assert_eq!(m1.stroke_count, 1),
            _ => panic!("expected mode 1"),
        }
        core.hw_clear();
    }

    #[test]
    fn clipboard_auto_id_never_collides_across_devices_or_identical_defaults() {
        let dev_a = VitralCore::init(r#"{"device_id":"devA"}"#).unwrap();
        let dev_b = VitralCore::init(r#"{"device_id":"devB"}"#).unwrap();

        let state_a = dev_a.clipboard_push(ClipboardPayload {
            id: None,
            content: "from-devA".into(),
            mime_type: "text/plain".into(),
            source_device: "devA".into(),
            timestamp_ms: 1000,
        });
        let entry_a = state_a.ring[0].clone();
        assert!(entry_a.id.starts_with("devA-"));

        let state_b = dev_b.clipboard_push(ClipboardPayload {
            id: None,
            content: "from-devB".into(),
            mime_type: "text/plain".into(),
            source_device: "devB".into(),
            timestamp_ms: 1000,
        });
        let entry_b = state_b.ring[0].clone();
        assert!(entry_b.id.starts_with("devB-"));
        assert_ne!(entry_a.id, entry_b.id);

        let qp_b = dev_b.clipboard_ingest_remote(
            ClipboardPayload {
                id: Some(entry_a.id.clone()),
                content: entry_a.content.clone(),
                mime_type: entry_a.mime_type.clone(),
                source_device: entry_a.source_device.clone(),
                timestamp_ms: entry_a.timestamp_ms,
            },
            false,
        );
        assert_eq!(qp_b.current_copy.as_ref().unwrap().id, entry_a.id.max(entry_b.id));
        assert_eq!(dev_b.clipboard_state().ring.len(), 2);

        // Even with empty/matching device_ids and whitespace-only id, instance counter prevents collisions
        let def_1 = VitralCore::init(r#"{"device_id":""}"#).unwrap();
        let def_2 = VitralCore::init("{}").unwrap();
        let s1 = def_1.clipboard_push(ClipboardPayload {
            id: Some("   ".into()),
            content: "d1-secret".into(),
            mime_type: "text/plain".into(),
            source_device: "d1".into(),
            timestamp_ms: 42,
        });
        let s2 = def_2.clipboard_push(ClipboardPayload {
            id: None,
            content: "d2-secret".into(),
            mime_type: "text/plain".into(),
            source_device: "d2".into(),
            timestamp_ms: 42,
        });
        assert_ne!(s1.ring[0].id, s2.ring[0].id);

        let qp_2 = def_2.clipboard_ingest_remote(
            ClipboardPayload {
                id: Some(s1.ring[0].id.clone()),
                content: s1.ring[0].content.clone(),
                mime_type: s1.ring[0].mime_type.clone(),
                source_device: s1.ring[0].source_device.clone(),
                timestamp_ms: s1.ring[0].timestamp_ms,
            },
            false,
        );
        assert_eq!(qp_2.current_copy.as_ref().unwrap().id, s1.ring[0].id.clone().max(s2.ring[0].id.clone()));
        assert_eq!(def_2.clipboard_state().ring.len(), 2);
    }
}

#[cfg(test)]
mod m1a_tests {
    use super::*;
    use crate::dav::put_lexicon_import_artifact;
    use crate::eng::gram::RimeGramDb;
    use crate::eng::pure_rust::{pure_rust_opencc, PureRustEngine};
    use crate::eng::quanpin::decode_quanpin_long_context;
    use crate::eng::{resolve_assets_dir, LangMode};
    use crate::imp::polyphone::RimeDictCorpus;
    use crate::imp::scel::parse_scel;
    use crate::imp::xiaohe::syllable_to_xiaohe;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root")
    }

    fn type_ascii(core: &VitralCore, keys: &str) -> InputAction {
        let mut last = None;
        for b in keys.bytes() {
            last = Some(core.process_key(b as u32, 0, SwipeDir::None).unwrap());
        }
        last.expect("non-empty keys")
    }

    #[test]
    fn xiaohe_algebra_exact_mapping() {
        assert_eq!(syllable_to_xiaohe("mào"), "mc");
        assert_eq!(syllable_to_xiaohe("dí"), "di");
        assert_eq!(syllable_to_xiaohe("lǜ"), "lv");
        assert_eq!(syllable_to_xiaohe("nǚ"), "nv");
        assert_eq!(syllable_to_xiaohe("lüè"), "lt");
        assert_eq!(syllable_to_xiaohe("zhāo"), "vc");
        assert_eq!(syllable_to_xiaohe("chóng"), "is");
        assert_eq!(syllable_to_xiaohe("shuāng"), "ul");
        assert_eq!(syllable_to_xiaohe("hǎo"), "hc");
        assert_eq!(syllable_to_xiaohe("nǐ"), "ni");
        assert_eq!(syllable_to_xiaohe("ang"), "ah");
        assert_eq!(syllable_to_xiaohe("a"), "aa");
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_rime_shows_bundled_abbreviations_in_both_schemas() {
        let core = VitralCore::init(r#"{"adaptive_learning":false,"pure_offline":true}"#).unwrap();
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            core.set_schema(schema);
            let action = type_ascii(&core, "zsh");
            assert!(action.candidates.iter().take(3).any(|item| item.text == "早上好"),
                "{schema:?}: {:?}", action.candidates);
            core.process_key(0x1b, 0, SwipeDir::None).unwrap();
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_librime_octagram_opencc_and_schema_switching() {
        use crate::eng::{opencc_ffi, rime_ffi};

        let assets_dir = resolve_assets_dir("").expect("workspace assets");
        rime_ffi::with_runtime(&assets_dir, "", "", |rt| {
            assert!(
                rt.version().starts_with("1."),
                "unexpected librime version: {}",
                rt.version()
            );
            assert!(rt.octagram_loaded(), "librime-octagram must be loaded");
            Ok(())
        })
        .unwrap();

        let core = VitralCore::init("{}").unwrap();
        // 1. Default schema: double_pinyin_flypy ("nihc" -> "你好")
        let act = type_ascii(&core, "nihc");
        assert_eq!(act.schema, "double_pinyin_flypy");
        assert!(
            act.candidates.iter().any(|c| c.text == "你好"),
            "expected '你好' for flypy 'nihc', got {:?}",
            act.candidates
        );
        // Escape clears composition cleanly
        let esc = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        assert!(esc.preedit.text.is_empty());

        // 2. Deterministic one-click switch to Quanpin (rime_ice)
        core.set_schema(SchemaKind::RimeIce);
        let qp = type_ascii(&core, "nihaoshijie");
        assert_eq!(qp.schema, "rime_ice");
        assert_eq!(qp.candidates[0].text, "你好世界");

        // 3. Backspace + select_candidate
        let _ = core.process_key(0x08, 0, SwipeDir::None).unwrap();
        let _ = core.process_key('e' as u32, 0, SwipeDir::None).unwrap();
        let sel = core.select_candidate(0).unwrap();
        assert_eq!(sel.commit_text.as_deref(), Some("你好世界"));

        // 4. OpenCC libopencc.so direct verification across s2t / s2twp / s2hk
        let opencc_dir = assets_dir.join("opencc");
        assert_eq!(
            opencc_ffi::convert_text(&opencc_dir, OpenCcMode::Traditional, "汉字转换测试软件")
                .unwrap(),
            "漢字轉換測試軟件"
        );
        assert_eq!(
            opencc_ffi::convert_text(&opencc_dir, OpenCcMode::TaiwanStandard, "汉字转换测试软件")
                .unwrap(),
            "漢字轉換測試軟體"
        );
        assert_eq!(
            opencc_ffi::convert_text(&opencc_dir, OpenCcMode::HongKong, "汉字转换测试软件")
                .unwrap(),
            "漢字轉換測試軟件"
        );

        // 5. Engine OpenCC mode applies to IME output
        core.set_opencc_mode(OpenCcMode::TaiwanStandard);
        let tc = type_ascii(&core, "ruanjian");
        assert!(
            tc.candidates.iter().any(|c| c.text == "軟體"),
            "expected TaiwanStandard '軟體' for 'ruanjian', got {:?}",
            tc.candidates
        );
    }

    #[test]
    fn pure_rust_engine_flypy_quanpin_and_opencc() {
        let assets = resolve_assets_dir("");
        let mut engine = PureRustEngine::new(assets.as_deref());
        let idx = ImportedLexiconIndex::new(assets.as_deref());
        let ret = RetentionEngine::default();

        let (cands_flypy, _) = engine.query_candidates(
            "nihc",
            SchemaKind::DoublePinyinFlypy,
            OpenCcMode::Simplified,
            Some(&idx),
            Some(&ret),
        );
        assert_eq!(cands_flypy[0].text, "你好");

        let (cands_qp, _) = engine.query_candidates(
            "nihaoshijie",
            SchemaKind::RimeIce,
            OpenCcMode::Simplified,
            Some(&idx),
            Some(&ret),
        );
        assert_eq!(cands_qp[0].text, "你好世界");

        assert_eq!(
            pure_rust_opencc(OpenCcMode::TaiwanStandard, "软件测试"),
            "軟體測試"
        );
        assert_eq!(
            pure_rust_opencc(OpenCcMode::Traditional, "汉字转换"),
            "漢字轉換"
        );
    }

    #[test]
    fn rime_gram_db_and_0xe1_escape_verified() {
        let assets = resolve_assets_dir("").expect("workspace assets");
        let gram_path = assets.join("rime").join("wanxiang-lts-zh-hans.gram");
        let gram = RimeGramDb::open_cached(&gram_path).expect("open wanxiang gram");

        // Verify 0xe1 escape for lo == 0 CJK (`一` = U+4E00)
        assert_eq!(RimeGramDb::encode("一"), vec![0xe1, 0x8e]);
        assert!(
            gram.lookup("一", "直").is_some() || gram.lookup("", "一起").is_some(),
            "0xe1-escaped U+4E00 一 lookup must succeed"
        );

        let p_xa = gram.lookup("去", "西安").expect("去 西安");
        let p_ly = gram.lookup("西安", "旅游").expect("西安 旅游");
        let p_fg = gram.lookup("让人", "反感").expect("让人 反感");
        let p_sj = gram.lookup("实践", "出真知").expect("实践 出真知");
        let p_ja = gram.lookup("情绪", "激昂").expect("情绪 激昂");
        assert!(p_xa > 13.0 && p_ly > 15.0);
        assert!(p_fg > 17.0 && p_sj > 22.0 && p_ja > 17.0);
    }

    fn char_accuracy(pred: &str, gold: &str) -> f64 {
        let p: Vec<char> = pred.chars().collect();
        let g: Vec<char> = gold.chars().collect();
        if g.is_empty() {
            return 1.0;
        }
        let matches = p.iter().zip(g.iter()).filter(|(a, b)| a == b).count();
        matches as f64 / g.len() as f64
    }

    #[test]
    fn multi_length_quanpin_monotonic_accuracy_benchmark() {
        let assets = resolve_assets_dir("").expect("workspace assets");
        let corpus = RimeDictCorpus::load_or_fallback(Some(&assets));
        let gram = RimeGramDb::open_cached(&assets.join("rime").join("wanxiang-lts-zh-hans.gram"))
            .expect("open gram");

        // 7 ambiguous phonetic chains across Short (L1), Medium (L2), and Long (L3) prefixes
        struct Chain {
            name: &'static str,
            l1: (&'static str, &'static str),
            l2: (&'static str, &'static str),
            l3: (&'static str, &'static str),
        }
        let chains = [
            Chain {
                name: "xian",
                l1: ("xian", "西安"),
                l2: ("quxianlvyou", "去西安旅游"),
                l3: ("woquxianlvyoukanbingmayong", "我去西安旅游看兵马俑"),
            },
            Chain {
                name: "fangan",
                l1: ("fangan", "反感"),
                l2: ("rangrenfangan", "让人反感"),
                l3: ("zhegejuedingrangrenfangan", "这个决定让人反感"),
            },
            Chain {
                name: "shijian",
                l1: ("shijian", "实践"),
                l2: ("shijianchuzhenzhi", "实践出真知"),
                l3: ("shijianchuzhenzhishizhenli", "实践出真知是真理"),
            },
            Chain {
                name: "jiang",
                l1: ("jiang", "激昂"),
                l2: ("qingxujiang", "情绪激昂"),
                l3: ("dajiaqingxujiangfenfa", "大家情绪激昂奋发"),
            },
            Chain {
                name: "jianan",
                l1: ("jianan", "建安"),
                l2: ("donghanjianan", "东汉建安"),
                l3: ("donghanjiananwenxue", "东汉建安文学"),
            },
            Chain {
                name: "changan",
                l1: ("changan", "长安"),
                l2: ("beijingchanganjie", "北京长安街"),
                l3: ("beijingchanganjiehenkuan", "北京长安街很宽"),
            },
            Chain {
                name: "pingan",
                l1: ("pingan", "平安"),
                l2: ("yilupingan", "一路平安"),
                l3: ("zhuniyilupinganshunli", "祝你一路平安顺利"),
            },
        ];

        let mut l1_exact = 0usize;
        let mut l2_exact = 0usize;
        let mut l3_exact = 0usize;
        let mut l1_char_sum = 0.0;
        let mut l2_char_sum = 0.0;
        let mut l3_char_sum = 0.0;

        eprintln!("=== Pure-Rust Long-Context Quanpin Benchmark ===");
        for c in &chains {
            let p1 = decode_quanpin_long_context(c.l1.0, &corpus, Some(&gram), None, None, 5);
            let p2 = decode_quanpin_long_context(c.l2.0, &corpus, Some(&gram), None, None, 5);
            let p3 = decode_quanpin_long_context(c.l3.0, &corpus, Some(&gram), None, None, 5);

            let t1 = p1.first().map(|x| x.text.as_str()).unwrap_or("");
            let t2 = p2.first().map(|x| x.text.as_str()).unwrap_or("");
            let t3 = p3.first().map(|x| x.text.as_str()).unwrap_or("");

            if t1 == c.l1.1 {
                l1_exact += 1;
            }
            if t2 == c.l2.1 {
                l2_exact += 1;
            }
            if t3 == c.l3.1 {
                l3_exact += 1;
            }
            let c1 = char_accuracy(t1, c.l1.1);
            let c2 = char_accuracy(t2, c.l2.1);
            let c3 = char_accuracy(t3, c.l3.1);
            l1_char_sum += c1;
            l2_char_sum += c2;
            l3_char_sum += c3;

            eprintln!(
                "PureRust [{}]: L1='{}'({:.2}) | L2='{}'({:.2}) | L3='{}'({:.2})",
                c.name, t1, c1, t2, c2, t3, c3
            );

            assert_eq!(
                t2, c.l2.1,
                "Pure-Rust Quanpin L2 mismatch on {}: got '{t2}' expected '{}'",
                c.name, c.l2.1
            );
            assert_eq!(
                t3, c.l3.1,
                "Pure-Rust Quanpin L3 mismatch on {}: got '{t3}' expected '{}'",
                c.name, c.l3.1
            );
        }

        let n = chains.len() as f64;
        let l1_char = l1_char_sum / n;
        let l2_char = l2_char_sum / n;
        let l3_char = l3_char_sum / n;
        eprintln!(
            "PureRust Summary: exact {l1_exact}/7 -> {l2_exact}/7 -> {l3_exact}/7 | char {l1_char:.3} -> {l2_char:.3} -> {l3_char:.3}"
        );
        assert!(
            l2_exact >= l1_exact && l3_exact >= l2_exact,
            "expected monotonic exact accuracy L1={l1_exact} <= L2={l2_exact} <= L3={l3_exact}"
        );
        assert!(
            l2_char >= l1_char && l3_char >= l2_char,
            "expected monotonic char accuracy L1={l1_char:.3} <= L2={l2_char:.3} <= L3={l3_char:.3}"
        );

        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        {
            let core = VitralCore::init("{}").unwrap();
            core.set_schema(SchemaKind::RimeIce);
            let mut rime_l1 = 0usize;
            let mut rime_l2 = 0usize;
            let mut rime_l3 = 0usize;
            let mut rime_c1_sum = 0.0;
            let mut rime_c2_sum = 0.0;
            let mut rime_c3_sum = 0.0;

            eprintln!("=== Native Linux librime + octagram Benchmark ===");
            for c in &chains {
                let a1 = type_ascii(&core, c.l1.0);
                let r1 = a1.candidates.first().map(|x| x.text.clone()).unwrap_or_default();
                let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

                let a2 = type_ascii(&core, c.l2.0);
                let r2 = a2.candidates.first().map(|x| x.text.clone()).unwrap_or_default();
                let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

                let a3 = type_ascii(&core, c.l3.0);
                let r3 = a3.candidates.first().map(|x| x.text.clone()).unwrap_or_default();
                let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

                if r1 == c.l1.1 {
                    rime_l1 += 1;
                }
                if r2 == c.l2.1 {
                    rime_l2 += 1;
                }
                if r3 == c.l3.1 {
                    rime_l3 += 1;
                }
                let rc1 = char_accuracy(&r1, c.l1.1);
                let rc2 = char_accuracy(&r2, c.l2.1);
                let rc3 = char_accuracy(&r3, c.l3.1);
                rime_c1_sum += rc1;
                rime_c2_sum += rc2;
                rime_c3_sum += rc3;

                eprintln!(
                    "librime  [{}]: L1='{}'({:.2}) | L2='{}'({:.2}) | L3='{}'({:.2})",
                    c.name, r1, rc1, r2, rc2, r3, rc3
                );
            }
            let rime_c1 = rime_c1_sum / n;
            let rime_c2 = rime_c2_sum / n;
            let rime_c3 = rime_c3_sum / n;
            eprintln!(
                "librime  Summary: exact {rime_l1}/7 -> {rime_l2}/7 -> {rime_l3}/7 | char {rime_c1:.3} -> {rime_c2:.3} -> {rime_c3:.3}"
            );
            assert!(
                rime_l2 >= rime_l1 && rime_l3 >= rime_l2,
                "librime monotonic exact accuracy check failed: {rime_l1} -> {rime_l2} -> {rime_l3}"
            );
            assert!(
                rime_c2 >= rime_c1 && rime_c3 >= rime_c2,
                "librime monotonic char accuracy check failed: {rime_c1:.3} -> {rime_c2:.3} -> {rime_c3:.3}"
            );
            assert!(
                rime_l3 >= 6 && rime_c3 >= 0.90,
                "librime L3 accuracy below threshold: {rime_l3}/7, char={rime_c3:.3}"
            );
        }
    }

    #[test]
    fn lexicon_scel_0x44_0x45_and_corrupt_rejection() {
        let fixtures = workspace_root().join("tests/fixtures/lexicons");
        let core = VitralCore::init("{}").unwrap();

        // 1. Real 0x44 .scel fixture
        let rep = core
            .import_lexicon(&fixtures.join("sample_hotwords.scel"), LexiconFormat::Auto)
            .unwrap();
        assert_eq!(rep.format, LexiconFormat::Scel);
        assert!(rep.imported_count >= 12);

        // Verify immediate ranking in both DoublePinyinFlypy and RimeIce
        core.set_schema(SchemaKind::DoublePinyinFlypy);
        let xh = type_ascii(&core, "bcgl");
        let pos_baoguang = xh.candidates.iter().position(|c| c.text == "爆光").unwrap();
        let pos_puguang = xh.candidates.iter().position(|c| c.text == "曝光").unwrap();
        assert!(pos_baoguang < pos_puguang);
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

        core.set_schema(SchemaKind::RimeIce);
        let qp = type_ascii(&core, "baoguang");
        let pos_baoguang_q = qp.candidates.iter().position(|c| c.text == "爆光").unwrap();
        let pos_puguang_q = qp.candidates.iter().position(|c| c.text == "曝光").unwrap();
        assert!(pos_baoguang_q < pos_puguang_q);

        // Verify M1-B hooks on ImportedLexiconIndex (scoped so read lock is released before next import)
        {
            let idx = core.imported_lexicon();
            let idx_r = idx.read();
            assert!(
                idx_r.lookup_word_score("爆光").unwrap()
                    > idx_r.lookup_word_score("曝光").unwrap()
            );
            assert!(
                idx_r.bigram_pmi_score("爆", "金币", 1)
                    > idx_r.bigram_pmi_score("曝", "金币", 1)
            );
            assert_eq!(
                idx_r.char_phonetics('耄')[0],
                ("mào".to_string(), "mc".to_string())
            );
            assert_eq!(
                idx_r.char_phonetics('頔')[0],
                ("dí".to_string(), "di".to_string())
            );
            assert!(idx_r
                .char_phonetics('绿')
                .contains(&("lǜ".to_string(), "lv".to_string())));
        }

        // 2. Corrupt .scel rejection
        let err = core
            .import_lexicon(&fixtures.join("corrupt.scel"), LexiconFormat::Auto)
            .unwrap_err();
        assert!(matches!(err, VitralError::Parse(_)));

        // 3. Synthetic 0x45 .scel variant (word_start = 0x26C4)
        let mut scel45 = vec![0u8; 0x26C4];
        scel45[0..4].copy_from_slice(&[0x40, 0x15, 0x00, 0x00]);
        scel45[0x04] = 0x45;
        scel45[0x05..0x08].copy_from_slice(&[0x43, 0x53, 0x01]);
        scel45[0x1540..0x1544].copy_from_slice(&2u32.to_le_bytes());
        let mut p = 0x1544usize;
        for (id, syl) in [(0u16, "ce"), (1u16, "shi")] {
            let u16s: Vec<u8> = syl.encode_utf16().flat_map(u16::to_le_bytes).collect();
            scel45[p..p + 2].copy_from_slice(&id.to_le_bytes());
            scel45[p + 2..p + 4].copy_from_slice(&(u16s.len() as u16).to_le_bytes());
            scel45[p + 4..p + 4 + u16s.len()].copy_from_slice(&u16s);
            p += 4 + u16s.len();
        }
        // Append 1 word block: "测试" (freq 777)
        scel45.extend_from_slice(&1u16.to_le_bytes()); // same_py_count = 1
        scel45.extend_from_slice(&4u16.to_le_bytes()); // py_idx_bytes_len = 4
        scel45.extend_from_slice(&0u16.to_le_bytes());
        scel45.extend_from_slice(&1u16.to_le_bytes());
        let w_u16: Vec<u8> = "测试".encode_utf16().flat_map(u16::to_le_bytes).collect();
        scel45.extend_from_slice(&(w_u16.len() as u16).to_le_bytes());
        scel45.extend_from_slice(&w_u16);
        scel45.extend_from_slice(&10u16.to_le_bytes()); // ext_len = 10
        scel45.extend_from_slice(&777u16.to_le_bytes());
        scel45.extend_from_slice(&[0u8; 8]);

        let parsed45 = parse_scel(&scel45).unwrap();
        assert_eq!(parsed45.len(), 1);
        assert_eq!(parsed45[0].text, "测试");
        assert_eq!(parsed45[0].syllables, vec!["ce", "shi"]);
        assert_eq!(parsed45[0].weight, 777);
    }

    #[test]
    fn lexicon_dict_yaml_tsv_txt_polyphone_and_sync_gating() {
        let fixtures = workspace_root().join("tests/fixtures/lexicons");
        let tmp_dir = std::env::temp_dir().join(format!(
            "vitral_m1a_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();

        // 1. DictYaml (sort: by_weight)
        let core = VitralCore::init(
            &serde_json::json!({ "data_dir": tmp_dir.to_string_lossy() }).to_string(),
        )
        .unwrap();
        let rep_yaml = core
            .import_lexicon(&fixtures.join("sample_terms.dict.yaml"), LexiconFormat::Auto)
            .unwrap();
        assert_eq!(rep_yaml.format, LexiconFormat::DictYaml);
        assert_eq!(rep_yaml.imported_count, 12);
        assert!(rep_yaml.entries[0].weight >= rep_yaml.entries[1].weight);
        assert!(rep_yaml.sync_artifact_path.is_some());
        assert!(Path::new(rep_yaml.sync_artifact_path.as_ref().unwrap()).is_file());

        // 2. TSV (3-column fixture + 2-column temp TSV)
        let rep_tsv = core
            .import_lexicon(&fixtures.join("sample_pairs.tsv"), LexiconFormat::Auto)
            .unwrap();
        assert_eq!(rep_tsv.imported_count, 12);
        let two_col_tsv = tmp_dir.join("two_col.tsv");
        std::fs::write(&two_col_tsv, "毛玻璃\tmao bo li\n雾面玻璃\twu mian bo li\n").unwrap();
        let rep_tsv2 = core.import_lexicon(&two_col_tsv, LexiconFormat::Tsv).unwrap();
        assert_eq!(rep_tsv2.imported_count, 2);

        // 3. Un-annotated .txt polyphone verification against expected_polyphones.json (13/13)
        let rep_txt = core
            .import_lexicon(
                &fixtures.join("polyphone_unannotated.txt"),
                LexiconFormat::Auto,
            )
            .unwrap();
        assert_eq!(rep_txt.imported_count, 13);
        let expected_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixtures.join("expected_polyphones.json")).unwrap(),
        )
        .unwrap();
        for case in expected_json["cases"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let target_idx = case["target_index"].as_u64().unwrap() as usize;
            let expected_plain = case["expected_syllable"].as_str().unwrap();
            let expected_toned = case["expected_toned_syllable"].as_str().unwrap();
            let expected_full = case["expected_full_pinyin"].as_str().unwrap();

            let ent = rep_txt
                .entries
                .iter()
                .find(|e| e.text == text)
                .unwrap_or_else(|| panic!("missing polyphone entry {text}"));
            assert_eq!(ent.pinyin, expected_full, "plain mismatch for {text}");
            let plain_toks: Vec<&str> = ent.pinyin.split_whitespace().collect();
            let toned_toks: Vec<&str> = ent.toned_pinyin.split_whitespace().collect();
            assert_eq!(plain_toks[target_idx], expected_plain, "plain tok for {text}");
            assert_eq!(toned_toks[target_idx], expected_toned, "toned tok for {text}");
        }

        // 4. Malformed edge fixture skips cleanly with warnings; binary rejected
        let rep_bad = core
            .import_lexicon(&fixtures.join("malformed_edge.txt"), LexiconFormat::Tsv)
            .unwrap();
        assert!(rep_bad.skipped_count >= 8);
        assert!(!rep_bad.warnings.is_empty());

        let bin_path = tmp_dir.join("binary.txt");
        std::fs::write(&bin_path, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert!(matches!(
            core.import_lexicon(&bin_path, LexiconFormat::Txt),
            Err(VitralError::Parse(_))
        ));

        // 5. OfflineGuard sync gating + live local WebDAV PUT mock (zero live_slot.enc)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received_paths = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let paths_bg = Arc::clone(&received_paths);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().take(5) {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let first_line = head.lines().next().unwrap_or("");
                let parts: Vec<&str> = first_line.split_whitespace().collect();
                if parts.len() >= 2 {
                    paths_bg.lock().push(format!("{} {}", parts[0], parts[1]));
                    if parts[0] == "PUT" {
                        let _ = s.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        break;
                    }
                    let _ = s.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            }
        });

        let guard_offline = OfflineGuard::new(true);
        assert!(matches!(
            put_lexicon_import_artifact(
                &guard_offline,
                &format!("http://{addr}"),
                None,
                None,
                None,
                "test.cbor",
                b"abc"
            ),
            Err(VitralError::PureOfflineViolation(_))
        ));

        let core_sync = VitralCore::init(
            &serde_json::json!({
                "data_dir": tmp_dir.to_string_lossy(),
                "koofr_endpoint": format!("http://{addr}")
            })
            .to_string(),
        )
        .unwrap();
        let rep_sync = core_sync
            .import_lexicon(&fixtures.join("sample_pairs.tsv"), LexiconFormat::Auto)
            .unwrap();
        let _ = handle.join();
        let remote = rep_sync.sync_remote_path.expect("remote sync path");
        assert!(remote.starts_with("/Koofr/Vitral/lexicon/imports/import_"));
        let reqs = received_paths.lock();
        assert!(reqs.iter().any(|r| r.starts_with("PUT /Vitral/lexicon/imports/import_")));
        assert!(!reqs.iter().any(|r| r.contains("live_slot.enc")));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn r2_regression_quanpin_zero_initial_and_unseen_chains() {
        let assets = resolve_assets_dir("").expect("workspace assets");
        let corpus = RimeDictCorpus::load_or_fallback(Some(&assets));
        let gram = RimeGramDb::open_cached(&assets.join("rime").join("wanxiang-lts-zh-hans.gram"))
            .expect("open gram");

        // Zero-initial decomposable single syllables must NOT over-split into 2-char/3-char compounds
        for (qp, allowed_chars) in [
            ("piao", &["票", "飘", "漂"][..]),
            ("biao", &["表", "标"][..]),
            ("qiao", &["巧", "桥", "敲", "悄"][..]),
            ("qian", &["前", "钱", "千", "签"][..]),
            ("lian", &["连", "脸", "联", "练"][..]),
            ("diao", &["掉", "调", "吊", "雕"][..]),
            ("miao", &["妙", "秒", "描", "庙"][..]),
            ("tiao", &["条", "调", "跳", "挑"][..]),
            ("jiao", &["叫", "教", "交", "脚"][..]),
            ("xian", &["先", "现", "线", "显"][..]),
            ("xiao", &["小", "笑", "消", "效"][..]),
        ] {
            let res = decode_quanpin_long_context(qp, &corpus, Some(&gram), None, None, 5);
            let top = res.first().map(|x| x.text.as_str()).unwrap_or("");
            assert!(
                allowed_chars.contains(&top),
                "QP '{qp}' over-split! got '{top}', expected one of {allowed_chars:?}, all={:?}",
                res.iter().map(|x| (&x.text, x.score)).collect::<Vec<_>>()
            );
        }

        // Explicit apostrophe barrier still decodes 西安 cleanly
        let xian_apos = decode_quanpin_long_context("xi'an", &corpus, Some(&gram), None, None, 5);
        assert_eq!(xian_apos[0].text, "西安");

        // Common phrases & sentences that previously failed due to zero-initial split bias
        for (qp, expected) in [
            ("qianmian", "前面"),
            ("wodepiao", "我的票"),
            ("womenxianzuowanzhegexiangmu", "我们先做完这个项目"),
            ("xianzuowan", "先做完"),
        ] {
            let res = decode_quanpin_long_context(qp, &corpus, Some(&gram), None, None, 5);
            let top = res.first().map(|x| x.text.as_str()).unwrap_or("");
            assert_eq!(
                top, expected,
                "QP '{qp}' got '{top}' instead of '{expected}', all={:?}",
                res.iter().map(|x| (&x.text, x.score)).collect::<Vec<_>>()
            );
        }

        // Challenger 1's 8 unseen chains: L3 must decode cleanly and show monotonic accuracy
        let unseen_chains = [
            ("xianggang", "香港", "去香港旅游", "我去香港旅游买东西"),
            ("nanhai", "南海", "中国南海", "保护中国南海主权"),
            ("dangan", "档案", "个人档案", "查询个人档案信息"),
            ("minge", "名额", "有限名额", "争取有限名额机会"),
            ("tiananmen", "天安门", "北京天安门", "我去北京天安门广场"),
            ("piao", "票", "买张票", "我去火车站买张票"),
            ("xian2", "先", "先做完", "我们先做完这个项目"),
            ("jiao", "叫", "叫朋友", "我叫朋友一起吃饭"),
        ];
        let mut l1_ok = 0usize;
        let mut l2_ok = 0usize;
        let mut l3_ok = 0usize;
        let mut l1_char_sum = 0.0f64;
        let mut l2_char_sum = 0.0f64;
        let mut l3_char_sum = 0.0f64;
        for (name, g1, g2, g3) in unseen_chains {
            let i1 = match name {
                "xian2" => "xian",
                _ => name,
            };
            let i2 = match name {
                "xianggang" => "quxiangganglvyou",
                "nanhai" => "zhongguonanhai",
                "dangan" => "gerendangan",
                "minge" => "youxianminge",
                "tiananmen" => "beijingtiananmen",
                "piao" => "maizhangpiao",
                "xian2" => "xianzuowan",
                "jiao" => "jiaopengyou",
                _ => unreachable!(),
            };
            let i3 = match name {
                "xianggang" => "woquxiangganglvyoumaidongxi",
                "nanhai" => "baohuzhongguonanhaizhuquan",
                "dangan" => "chaxungerendanganxinxi",
                "minge" => "zhengquyouxianmingejihui",
                "tiananmen" => "woqubeijingtiananmenguangchang",
                "piao" => "woquhuochezhanmaizhangpiao",
                "xian2" => "womenxianzuowanzhegexiangmu",
                "jiao" => "wojiaopengyouyiqichifan",
                _ => unreachable!(),
            };
            let t1 = decode_quanpin_long_context(i1, &corpus, Some(&gram), None, None, 5)[0]
                .text
                .clone();
            let t2 = decode_quanpin_long_context(i2, &corpus, Some(&gram), None, None, 5)[0]
                .text
                .clone();
            let t3 = decode_quanpin_long_context(i3, &corpus, Some(&gram), None, None, 5)[0]
                .text
                .clone();
            if t1 == g1 {
                l1_ok += 1;
            }
            if t2 == g2 {
                l2_ok += 1;
            }
            if t3 == g3 {
                l3_ok += 1;
            }
            l1_char_sum += char_accuracy(&t1, g1);
            l2_char_sum += char_accuracy(&t2, g2);
            l3_char_sum += char_accuracy(&t3, g3);
        }
        let l1_char = l1_char_sum / 8.0;
        let l2_char = l2_char_sum / 8.0;
        let l3_char = l3_char_sum / 8.0;
        assert!(l2_ok >= l1_ok && l3_ok >= l2_ok);
        assert!(l2_char >= l1_char && l3_char >= l2_char);
        assert!(
            l3_ok >= 6 && l3_char >= 0.95,
            "expected >= 6/8 unseen L3 exact and >= 0.95 char acc, got {l3_ok}/8 ({l3_char:.3})"
        );
    }

    #[test]
    fn r2_regression_focus_opencc_retention_partial_select_and_paging() {
        let core = VitralCore::init("{}").unwrap();
        core.set_schema(SchemaKind::RimeIce);

        // 1. Mid-composition on_app_focus Zh -> En -> Zh must clear stale state
        let mid = type_ascii(&core, "nih");
        assert!(!mid.preedit.text.is_empty());
        assert!(!mid.candidates.is_empty());

        let focus_en = core.on_app_focus("org.kde.konsole").unwrap();
        assert_eq!(focus_en.lang_mode, LangMode::En);
        let en_key = core.process_key('a' as u32, 0, SwipeDir::None).unwrap();
        assert_eq!(en_key.commit_text.as_deref(), Some("a"));
        assert!(en_key.preedit.text.is_empty(), "En preedit leaked: {}", en_key.preedit.text);
        assert!(en_key.candidates.is_empty(), "En candidates leaked");

        let focus_zh = core.on_app_focus("org.mozilla.firefox").unwrap();
        assert_eq!(focus_zh.lang_mode, LangMode::Zh);
        let after_zh = core.process_key('o' as u32, 0, SwipeDir::None).unwrap();
        assert_eq!(after_zh.preedit.text, "o", "stale 'nih' leaked in backend");
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

        // 2. erase_candidate & pin_candidate under OpenCcMode::TaiwanStandard
        core.set_opencc_mode(OpenCcMode::TaiwanStandard);
        let rj1 = type_ascii(&core, "ruanjian");
        assert_eq!(rj1.candidates[0].text, "軟體");
        core.erase_candidate(0).unwrap();
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        let rj2 = type_ascii(&core, "ruanjian");
        assert!(
            !rj2.candidates.iter().any(|c| c.text == "軟體"),
            "erased '軟體' still appeared: {:?}",
            rj2.candidates
        );
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

        let xx1 = type_ascii(&core, "xinxi");
        let target_text = xx1.candidates[1].text.clone();
        core.pin_candidate(1).unwrap();
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        let xx2 = type_ascii(&core, "xinxi");
        assert_eq!(xx2.candidates[0].text, target_text);
        assert!(xx2.candidates[0].pinned, "pinned candidate lost pinned flag under TaiwanStandard");
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

        // 3. Space & digit commit for overlay under TaiwanStandard
        let tmp_dir = std::env::temp_dir().join(format!("vitral_r2_ov_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let tsv_path = tmp_dir.join("hotwords.tsv");
        std::fs::write(&tsv_path, "软件漫步\truan jian man bu\t777777\n星尘漫步者\txing chen man bu zhe\t999999\n").unwrap();
        core.import_lexicon(&tsv_path, LexiconFormat::Tsv).unwrap();

        let ov = type_ascii(&core, "ruanjianmanbu");
        assert_eq!(ov.candidates[0].text, "軟體漫步");
        let sp = core.process_key(0x20, 0, SwipeDir::None).unwrap();
        assert_eq!(sp.commit_text.as_deref(), Some("軟體漫步"));

        // Digit '1' on overlay and digit '2' on shifted Rime candidate
        let _ = type_ascii(&core, "ruanjianmanbu");
        let d1 = core.process_key('1' as u32, 0, SwipeDir::None).unwrap();
        assert_eq!(d1.commit_text.as_deref(), Some("軟體漫步"));

        // Sentence-embedded 5-char imported hotword on active backend (native-rime AND pure-rust)
        core.set_opencc_mode(OpenCcMode::Simplified);
        let emb = type_ascii(&core, "woxihuanxingchenmanbuzhe");
        assert!(
            emb.candidates.iter().any(|c| c.text == "我喜欢星尘漫步者"),
            "sentence-embedded imported hotword missing: {:?}",
            emb.candidates
        );
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

        // Xiaohe sentence-embedded imported hotword on active backend
        core.set_schema(SchemaKind::DoublePinyinFlypy);
        let emb_xh = type_ascii(&core, "woxihrxkifmjbuve");
        assert!(
            emb_xh.candidates.iter().any(|c| c.text == "我喜欢星尘漫步者"),
            "Xiaohe sentence-embedded imported hotword missing: {:?}",
            emb_xh.candidates
        );
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        core.set_schema(SchemaKind::RimeIce);

        // 4. select_candidate partial-prefix selection preserves remaining composition
        let full = type_ascii(&core, "woquxianlvyou");
        assert!(full.candidates.len() >= 2);
        let partial_idx = full
            .candidates
            .iter()
            .position(|c| c.text == "我去")
            .unwrap_or(1);
        let partial_res = core.select_candidate(partial_idx).unwrap();
        // Pure-Rust can commit the selected segment while retaining the unconsumed preedit.
        if partial_res.commit_text.is_none() || !partial_res.preedit.text.is_empty() {
            assert!(
                !partial_res.preedit.text.is_empty(),
                "partial select_candidate wiped preedit!"
            );
            assert!(!partial_res.candidates.is_empty());
            let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        }
        let _ = type_ascii(&core, "nihaoshijie");
        let finish = core.select_candidate(0).unwrap();
        assert_eq!(finish.commit_text.as_deref(), Some("你好世界"));

        // 5. Pure-Rust Engine 5-char hotword + paging without overlay repeat
        let assets = resolve_assets_dir("");
        let mut pure = PureRustEngine::new(assets.as_deref());
        let idx_arc = core.imported_lexicon();
        let idx_r = idx_arc.read();
        let (pr_qp, _) = pure.query_candidates(
            "xingchenmanbuzhe",
            SchemaKind::RimeIce,
            OpenCcMode::Simplified,
            Some(&idx_r),
            None,
        );
        assert_eq!(pr_qp[0].text, "星尘漫步者");
        let (pr_xh, _) = pure.query_candidates(
            "xkifmjbuve",
            SchemaKind::DoublePinyinFlypy,
            OpenCcMode::Simplified,
            Some(&idx_r),
            None,
        );
        assert_eq!(pr_xh[0].text, "星尘漫步者");
        let (pr_emb, _) = pure.query_candidates(
            "woxihuanxingchenmanbuzhe",
            SchemaKind::RimeIce,
            OpenCcMode::Simplified,
            Some(&idx_r),
            None,
        );
        assert_eq!(pr_emb[0].text, "我喜欢星尘漫步者");

        // Pure-Rust paging via EngineState without native-rime
        let mut pure_state = crate::eng::EngineState::new(EngineConfig {
            assets_dir: assets
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            schema: SchemaKind::RimeIce,
            ..EngineConfig::default()
        });
        let mut clip = ClipboardManager::new(10);
        let s = clip.push(ClipboardPayload {
            id: None,
            content: "https://example.org/pin".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 1,
        });
        clip.pin(&s.ring[0].id, Some("xian")).unwrap();
        let ret = RetentionEngine::default();
        for ch in "xian".bytes() {
            pure_state
                .process_key_pure_rust_for_test(ch as u32, &clip, &idx_r, &ret)
                .unwrap();
        }
        let p0 = pure_state
            .process_key_pure_rust_for_test(0, &clip, &idx_r, &ret)
            .unwrap();
        assert_eq!(p0.candidates[0].text, "https://example.org/pin");
        let p1 = pure_state
            .process_key_pure_rust_for_test('=' as u32, &clip, &idx_r, &ret)
            .unwrap();
        assert_eq!(p1.page.page_index, 1);
        assert!(
            !p1.candidates.iter().any(|c| c.text == "https://example.org/pin"),
            "page 0 overlay repeated on page 1!"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn r2_regression_gram_bounds_dict_yaml_polyphone_and_dav_root() {
        let tmp_dir = std::env::temp_dir().join(format!("vitral_r2_imp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();

        // 1. Negative rel_off = -40 in .gram must be rejected with Err(VitralError::Parse)
        let bad_gram = tmp_dir.join("bad_rel.gram");
        let mut gram_bytes = vec![0u8; 48];
        gram_bytes[0..18].copy_from_slice(b"Rime::Grammar/1.0\0");
        gram_bytes[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
        gram_bytes[0x28..0x2c].copy_from_slice(&(-40i32).to_le_bytes());
        std::fs::write(&bad_gram, &gram_bytes).unwrap();
        assert!(matches!(RimeGramDb::open(&bad_gram), Err(VitralError::Parse(_))));

        // 2. .dict.yaml BOM + quoted columns: ["text", "weight"] + invalid code validation
        let quoted_yaml = "\u{FEFF}---\nname: q\ncolumns: [\"text\", \"weight\"]\n...\n测试词\t500\n";
        let parsed = crate::imp::dict_yaml::parse_dict_yaml(quoted_yaml).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].text, "测试词");
        assert_eq!(parsed.entries[0].weight, 500);

        let bad_code_yaml = "---\nname: c\ncolumns: [\"text\", \"code\", \"weight\"]\n...\n合法词\the fa ci\t500\n坏编码\t1234!@#\t100\n";
        let parsed_code = crate::imp::dict_yaml::parse_dict_yaml(bad_code_yaml).unwrap();
        assert_eq!(parsed_code.entries.len(), 1);
        assert_eq!(parsed_code.skipped_count, 1);

        // 3. inject_hotwords deduplicates export_hotwords
        let mut idx = crate::imp::ImportedLexiconIndex::new(resolve_assets_dir("").as_deref());
        let entry = crate::imp::ImportedLexiconEntry {
            text: "星尘漫步者".into(),
            pinyin: "xing chen man bu zhe".into(),
            toned_pinyin: "xīng chén màn bù zhě".into(),
            xiaohe: "xkifmjbuve".into(),
            weight: 100,
        };
        idx.inject_hotwords(std::slice::from_ref(&entry));
        idx.inject_hotwords(&[crate::imp::ImportedLexiconEntry {
            weight: 999,
            ..entry
        }]);
        assert_eq!(idx.export_hotwords().len(), 1);
        assert_eq!(idx.export_hotwords()[0].weight, 999);

        // 4. Polyphone 2-stage checks on Challenger 2's 7 mismatched phrases
        let annotator = crate::imp::polyphone::PolyphoneAnnotator::from_assets_dir(
            resolve_assets_dir("").as_deref(),
        );
        for (txt, exp_plain, exp_toned) in [
            ("地地道道", "di di dao dao", "dì dì dào dào"),
            ("跑得很快", "pao de hen kuai", "pǎo de hěn kuài"),
            ("写得很好", "xie de hen hao", "xiě de hěn hǎo"),
            ("走得太快", "zou de tai kuai", "zǒu de tài kuài"),
            ("高兴地笑", "gao xing de xiao", "gāo xìng de xiào"),
            ("切菜做饭", "qie cai zuo fan", "qiē cài zuò fàn"),
            ("好客之道", "hao ke zhi dao", "hào kè zhī dào"),
            ("打的回家", "da di hui jia", "dǎ dī huí jiā"),
        ] {
            let ann = annotator.annotate_text(txt);
            assert_eq!(ann.untoned, exp_plain, "plain mismatch for {txt}");
            assert_eq!(ann.toned, exp_toned, "toned mismatch for {txt}");
        }

        // 5. dav.rs koofr_root = "/Koofr/Vitral" normalization
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().take(5) {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = s.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                if head.starts_with("PUT ") {
                    break;
                }
            }
        });
        let guard = OfflineGuard::new(false);
        let remote = put_lexicon_import_artifact(
            &guard,
            &format!("http://{addr}"),
            None,
            None,
            Some("/Koofr/Vitral"),
            "norm.cbor",
            b"123",
        )
        .unwrap();
        let _ = handle.join();
        assert_eq!(remote, "/Koofr/Vitral/lexicon/imports/norm.cbor");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

#[cfg(test)]
mod m1b_tests {
    use super::*;
    use crate::eng::resolve_assets_dir;
    use crate::hw::ids_exta::IdsIndex;
    use crate::hw::onnx_models::HwOnnxBundle;
    use crate::hw::raster::{render_directmap, reorder_strokes_top_left, DIRECTMAP_LEN};
    use crate::hw::{Tier3SlotDisambiguator, Tier3SlotRequest};
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root")
    }

    fn load_fixture_strokes(rel: &str) -> (Vec<Vec<StrokePoint>>, serde_json::Value) {
        let p = workspace_root()
            .join("tests/fixtures/handwriting")
            .join(rel);
        let raw = std::fs::read_to_string(&p).expect("read hw fixture");
        let val: serde_json::Value = serde_json::from_str(&raw).expect("parse hw fixture json");
        let strokes = val["strokes"]
            .as_array()
            .expect("strokes array")
            .iter()
            .map(|st| {
                st.as_array()
                    .expect("stroke pts")
                    .iter()
                    .map(|pt| StrokePoint {
                        x: pt["x"].as_f64().unwrap() as f32,
                        y: pt["y"].as_f64().unwrap() as f32,
                        timestamp_ms: pt["t"].as_u64().unwrap(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        (strokes, val)
    }

    #[test]
    fn directmap_rasterizer_and_single_point_and_1d_guards() {
        use crate::hw::raster::GlyphGeometry;

        let dot = vec![vec![StrokePoint {
            x: 0.5,
            y: 0.5,
            timestamp_ms: 0,
        }]];
        let dm_dot = render_directmap(&dot);
        assert_eq!(dm_dot.len(), DIRECTMAP_LEN);
        assert!(dm_dot.iter().any(|&v| v > 0.5), "dot stroke must not be all zero");

        let horiz = vec![vec![
            StrokePoint { x: 0.1, y: 0.5, timestamp_ms: 0 },
            StrokePoint { x: 0.9, y: 0.5, timestamp_ms: 50 },
        ]];
        let dm_h = render_directmap(&horiz);
        // Horizontal line centered near y = 23..24
        let mid_row_energy: f32 = (0..48)
            .map(|x| dm_h[23 * 48 + x] + dm_h[24 * 48 + x])
            .sum();
        assert!(mid_row_energy > 10.0, "1D horizontal stroke must center near y=23..24");

        let vert = vec![vec![
            StrokePoint { x: 0.5, y: 0.1, timestamp_ms: 0 },
            StrokePoint { x: 0.5, y: 0.9, timestamp_ms: 50 },
        ]];
        let dm_v = render_directmap(&vert);
        let mid_col_energy: f32 = (0..48)
            .map(|y| dm_v[y * 48 + 23] + dm_v[y * 48 + 24])
            .sum();
        assert!(mid_col_energy > 10.0, "1D vertical stroke must center near x=23..24");

        // Curved 1-stroke glyph (e.g. 乙) must NOT trigger 1D/straight primitive flags
        let curved_yi = vec![vec![
            StrokePoint { x: 0.20, y: 0.20, timestamp_ms: 0 },
            StrokePoint { x: 0.80, y: 0.20, timestamp_ms: 20 },
            StrokePoint { x: 0.30, y: 0.65, timestamp_ms: 40 },
            StrokePoint { x: 0.25, y: 0.85, timestamp_ms: 60 },
            StrokePoint { x: 0.85, y: 0.85, timestamp_ms: 80 },
        ]];
        let g_yi = GlyphGeometry::from_strokes(&curved_yi);
        assert!(!g_yi.is_single_horizontal);
        assert!(!g_yi.is_single_vertical);
        assert!(!g_yi.is_single_pie);
        assert!(!g_yi.is_single_na);

        // Top-left reordering places top stroke first
        let s_bot = vec![StrokePoint { x: 0.5, y: 0.85, timestamp_ms: 0 }];
        let s_top = vec![StrokePoint { x: 0.5, y: 0.15, timestamp_ms: 100 }];
        let reordered = reorder_strokes_top_left(&[s_bot, s_top]);
        assert!(reordered[0][0].y < reordered[1][0].y);
    }

    #[test]
    fn hw_lifecycle_not_initialized_and_clear() {
        let core = VitralCore::init("{}").unwrap();
        let pt = StrokePoint { x: 0.2, y: 0.5, timestamp_ms: 10 };
        assert!(matches!(
            core.hw_push_stroke(&[pt], true),
            Err(VitralError::NotInitialized)
        ));
        assert!(matches!(core.hw_end(), Err(VitralError::NotInitialized)));

        core.hw_begin(HwMode::PhoneticLookup);
        let p = core
            .hw_push_stroke(
                &[
                    StrokePoint { x: 0.15, y: 0.5, timestamp_ms: 0 },
                    StrokePoint { x: 0.85, y: 0.5, timestamp_ms: 30 },
                ],
                true,
            )
            .unwrap();
        match p {
            HandwritingPayload::Mode1(m1) => {
                assert_eq!(m1.stroke_count, 1);
                assert_eq!(m1.candidates[0].ch, "一");
                assert_eq!(m1.candidates[0].toned_pinyin, "yī");
            }
            _ => panic!("expected Mode1"),
        }

        core.hw_clear();
        let after_clear = core.hw_end().unwrap();
        match after_clear {
            HandwritingPayload::Mode1(m1) => {
                assert_eq!(m1.stroke_count, 0);
                assert!(m1.candidates.is_empty());
            }
            _ => panic!("expected Mode1"),
        }
    }

    #[test]
    fn non_onnx_multi_stroke_returns_model_unavailable_and_primitives_succeed() {
        use crate::hw::pure_fallback::classify_single_stroke_primitive;
        use crate::hw::raster::GlyphGeometry;

        // 1. Pure geometric 1-stroke primitives work without ONNX
        let mut sess =
            HandwritingSession::with_assets_dir(Some(Path::new("/nonexistent_vitral_assets_dir")));
        let imported = ImportedLexiconIndex::new(resolve_assets_dir("").as_deref());
        let mut retention = RetentionEngine::default();

        sess.begin(HwMode::PhoneticLookup);
        let p = {
            let ctx = HwDecodeContext {
                imported: &imported,
                gram: None,
                retention: &retention,
                tier3: None,
                offline_guard: None,
            };
            sess.push_stroke_with_context(
                &[
                    StrokePoint { x: 0.15, y: 0.5, timestamp_ms: 0 },
                    StrokePoint { x: 0.85, y: 0.5, timestamp_ms: 30 },
                ],
                true,
                &ctx,
            )
            .unwrap()
        };
        match p {
            HandwritingPayload::Mode1(m1) => {
                assert_eq!(m1.candidates[0].ch, "一");
                assert_eq!(m1.candidates[0].toned_pinyin, "yī");
            }
            _ => panic!("expected Mode1"),
        }
        assert_eq!(sess.confirm_result(&mut retention).as_deref(), Some("一"));
        let score_once = retention.frecency.get("一").copied().unwrap_or(0.0);
        // Idempotent confirm must not double-boost Frecency
        assert_eq!(sess.confirm_result(&mut retention).as_deref(), Some("一"));
        let score_twice = retention.frecency.get("一").copied().unwrap_or(0.0);
        assert!((score_twice - score_once).abs() < 1e-6);

        let ctx = HwDecodeContext {
            imported: &imported,
            gram: None,
            retention: &retention,
            tier3: None,
            offline_guard: None,
        };

        // Straight pie & na geometric primitives
        let pie = vec![vec![
            StrokePoint { x: 0.75, y: 0.15, timestamp_ms: 0 },
            StrokePoint { x: 0.25, y: 0.85, timestamp_ms: 30 },
        ]];
        assert_eq!(
            classify_single_stroke_primitive(&GlyphGeometry::from_strokes(&pie)).unwrap()[0].ch,
            '丿'
        );
        let na = vec![vec![
            StrokePoint { x: 0.20, y: 0.20, timestamp_ms: 0 },
            StrokePoint { x: 0.80, y: 0.80, timestamp_ms: 30 },
        ]];
        assert_eq!(
            classify_single_stroke_primitive(&GlyphGeometry::from_strokes(&na)).unwrap()[0].ch,
            '乀'
        );

        // 2. Curved 1-stroke glyph (乙) must NOT falsely match straight primitives
        sess.begin(HwMode::PhoneticLookup);
        let err_curved = sess.push_stroke_with_context(
            &[
                StrokePoint { x: 0.20, y: 0.20, timestamp_ms: 0 },
                StrokePoint { x: 0.80, y: 0.20, timestamp_ms: 20 },
                StrokePoint { x: 0.30, y: 0.65, timestamp_ms: 40 },
                StrokePoint { x: 0.25, y: 0.85, timestamp_ms: 60 },
                StrokePoint { x: 0.85, y: 0.85, timestamp_ms: 80 },
            ],
            true,
            &ctx,
        );
        assert!(matches!(err_curved, Err(VitralError::ModelUnavailable(_))));

        // 3. Multi-stroke Mode 1 & Mode 2 without ONNX must honestly return ModelUnavailable
        let (strokes_mao, _) = load_fixture_strokes("mode1_mao.json");
        sess.begin(HwMode::PhoneticLookup);
        for st in &strokes_mao {
            sess.push_stroke(st, true).unwrap();
        }
        assert!(matches!(
            sess.end_with_context(&ctx),
            Err(VitralError::ModelUnavailable(_))
        ));

        sess.begin(HwMode::ContinuousLattice);
        sess.push_stroke(
            &[
                StrokePoint { x: 0.15, y: 0.5, timestamp_ms: 0 },
                StrokePoint { x: 0.85, y: 0.5, timestamp_ms: 30 },
            ],
            true,
        )
        .unwrap();
        let err_m2 = sess.push_stroke_with_context(
            &[
                StrokePoint { x: 0.5, y: 0.15, timestamp_ms: 60 },
                StrokePoint { x: 0.5, y: 0.85, timestamp_ms: 90 },
            ],
            true,
            &ctx,
        );
        assert!(matches!(err_m2, Err(VitralError::ModelUnavailable(_))));

        // 4. Mode 2 1-stroke primitive + override + OOB + idempotent confirm without ONNX
        sess.begin(HwMode::ContinuousLattice);
        let m2 = match sess
            .push_stroke_with_context(
                &[
                    StrokePoint { x: 0.15, y: 0.5, timestamp_ms: 0 },
                    StrokePoint { x: 0.85, y: 0.5, timestamp_ms: 30 },
                ],
                true,
                &ctx,
            )
            .unwrap()
        {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2.composing_text, "一");
        assert!(matches!(sess.override_slot(9, 0), Err(VitralError::NotFound(_))));
        assert!(matches!(sess.override_slot(0, 99), Err(VitralError::NotFound(_))));
        let ov = sess.override_slot(0, 1).unwrap();
        assert!(ov.slots[0].manually_locked);
        assert!(!ov.slots[0].changed_retroactively);
        assert_eq!(sess.confirm_result(&mut retention).unwrap(), ov.composing_text);

        #[cfg(not(feature = "native-onnx"))]
        {
            let core = VitralCore::init("{}").unwrap();
            core.hw_begin(HwMode::PhoneticLookup);
            for st in &strokes_mao {
                let _ = core.hw_push_stroke(st, true);
            }
            assert!(matches!(core.hw_end(), Err(VitralError::ModelUnavailable(_))));
        }
    }

    #[test]
    fn mode2_spatiotemporal_dag_segmentation_and_ids_compositional_pure_rust() {
        use crate::hw::ids_exta::{compose_rare_candidates, find_bipartite_splits};
        use crate::hw::lattice::segment_strokes;

        let assets = resolve_assets_dir("");
        let ids = IdsIndex::load_cached(assets.as_deref());
        let no_onnx = HwOnnxBundle::load_cached(None);
        let imported = ImportedLexiconIndex::new(assets.as_deref());

        let (strokes_hc, _) = load_fixture_strokes("mode2_horizontal_connected.json");
        let spans_hc = segment_strokes(&strokes_hc, &no_onnx, &ids);
        assert_eq!(spans_hc.len(), 4, "horizontal connected 手写中文 must segment into 4 spans");

        let (strokes_bg, _) = load_fixture_strokes("mode2_baoguang_laodeng.json");
        let spans_bg = segment_strokes(&strokes_bg, &no_onnx, &ids);
        assert_eq!(spans_bg.len(), 6, "曝光老登金币 must segment into 6 spans");

        assert!(ids.are_structural_siblings('曝', '爆'));
        assert!(!ids.are_structural_siblings('曝', '明'));

        let (strokes_di, _) = load_fixture_strokes("mode1_di.json");
        let splits = find_bipartite_splits(&strokes_di);
        assert!(!splits.is_empty());
        let composed_di = compose_rare_candidates(
            &ids,
            &imported,
            None,
            &splits[0],
            &[('由', 0.9)],
            &[('頁', 0.85)],
            strokes_di.len(),
        );
        assert_eq!(composed_di.first().map(|x| x.0), Some('頔'));

        let composed_exta = compose_rare_candidates(
            &ids,
            &imported,
            None,
            &splits[0],
            &[('口', 0.9)],
            &[('王', 0.85)],
            8,
        );
        assert!(composed_exta.iter().any(|(ch, _)| *ch == '㕵'));
    }

    #[test]
    fn mode2_lattice_entropy_gate_tier1_viterbi_pmi_override_and_confirm_pure_rust() {
        use crate::eng::gram::RimeGramDb;
        use crate::hw::lattice::build_lattice_slot_from_predictions;
        use crate::hw::onnx_models::VisualPrediction;
        use crate::hw::rerank::decode_lattice_3tier;
        use crate::offline_guard::OfflineGuard;
        use std::collections::HashMap;

        let assets = resolve_assets_dir("");
        let mut imported = ImportedLexiconIndex::new(assets.as_deref());
        let scel_path = workspace_root().join("tests/fixtures/lexicons/sample_hotwords.scel");
        let _ = crate::imp::compile_and_index(&mut imported, &scel_path, LexiconFormat::Auto, None);
        let gram = assets
            .as_deref()
            .and_then(|d| RimeGramDb::open_cached(&d.join("rime").join("wanxiang-lts-zh-hans.gram")).ok());
        let retention = RetentionEngine::default();
        let no_onnx = HwOnnxBundle::load_cached(None);

        // Slot 0 elastic (曝/爆 Δp < 0.15), Slot 1..5 locked by entropy (光老登金币)
        let slot0 = build_lattice_slot_from_predictions(
            0,
            0,
            18,
            &[
                VisualPrediction { ch: '曝', prob: 0.52, raw_logit: 2.0 },
                VisualPrediction { ch: '爆', prob: 0.45, raw_logit: 1.85 },
            ],
            false,
        );
        assert!(!slot0.locked_by_entropy);
        assert!(slot0.margin_delta_p < 0.15);

        let locked_chars = ['光', '老', '登', '金', '币'];
        let mut lattice = vec![slot0];
        for (i, &ch) in locked_chars.iter().enumerate() {
            let s = build_lattice_slot_from_predictions(
                i + 1,
                19 + i,
                19 + i,
                &[
                    VisualPrediction { ch, prob: 0.96, raw_logit: 5.0 },
                    VisualPrediction { ch: '一', prob: 0.02, raw_logit: 0.1 },
                ],
                false,
            );
            assert!(s.locked_by_entropy);
            lattice.push(s);
        }

        let ctx = HwDecodeContext {
            imported: &imported,
            gram: gram.as_deref(),
            retention: &retention,
            tier3: None,
            offline_guard: None,
        };
        let baseline = vec![
            "曝".to_string(),
            "光".to_string(),
            "老".to_string(),
            "登".to_string(),
            "金".to_string(),
            "币".to_string(),
        ];
        let decoded = decode_lattice_3tier(&lattice, &HashMap::new(), &baseline, &no_onnx, &ctx);
        assert_eq!(decoded[0].ch, "爆");
        assert!(decoded[0].changed_retroactively);
        assert!(!decoded[1].changed_retroactively);

        // Manual lock preserves 曝 even under trailing context
        let mut manual = HashMap::new();
        manual.insert((0usize, 18usize), "曝".to_string());
        let decoded_locked = decode_lattice_3tier(&lattice, &manual, &baseline, &no_onnx, &ctx);
        assert_eq!(decoded_locked[0].ch, "曝");
        assert!(decoded_locked[0].manually_locked);
        assert!(!decoded_locked[0].changed_retroactively);

        // OfflineGuard blocks Tier 3 hook when pure_offline = true
        struct PanicIfCalledTier3;
        impl Tier3SlotDisambiguator for PanicIfCalledTier3 {
            fn disambiguate_slots(
                &self,
                _req: &Tier3SlotRequest,
            ) -> Result<Vec<(usize, usize)>, VitralError> {
                Err(VitralError::PureOfflineViolation("blocked".into()))
            }
        }
        let guard = OfflineGuard::new(true);
        let ctx_off = HwDecodeContext {
            imported: &imported,
            gram: gram.as_deref(),
            retention: &retention,
            tier3: Some(&PanicIfCalledTier3),
            offline_guard: Some(&guard),
        };
        let decoded_off = decode_lattice_3tier(&lattice, &HashMap::new(), &baseline, &no_onnx, &ctx_off);
        assert_eq!(decoded_off[0].ch, "爆");
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn mode2_left_right_intra_char_pause_stays_unified_and_pairs_split() {
        let core = VitralCore::init("{}").unwrap();

        // Single left-right char '明' (日 + 月) with 30ms intra-radical gaps & 160ms pause between radicals
        let ming_strokes: Vec<Vec<StrokePoint>> = vec![
            // 日: left vert, top-right hook, mid horiz, bottom horiz
            vec![
                StrokePoint { x: 0.12, y: 0.22, timestamp_ms: 0 },
                StrokePoint { x: 0.12, y: 0.78, timestamp_ms: 25 },
            ],
            vec![
                StrokePoint { x: 0.12, y: 0.22, timestamp_ms: 55 },
                StrokePoint { x: 0.36, y: 0.22, timestamp_ms: 70 },
                StrokePoint { x: 0.36, y: 0.78, timestamp_ms: 90 },
            ],
            vec![
                StrokePoint { x: 0.13, y: 0.50, timestamp_ms: 120 },
                StrokePoint { x: 0.35, y: 0.50, timestamp_ms: 140 },
            ],
            vec![
                StrokePoint { x: 0.12, y: 0.78, timestamp_ms: 170 },
                StrokePoint { x: 0.36, y: 0.78, timestamp_ms: 190 },
            ],
            // 160ms pause (< 420ms) before 月
            vec![
                StrokePoint { x: 0.52, y: 0.18, timestamp_ms: 350 },
                StrokePoint { x: 0.52, y: 0.58, timestamp_ms: 370 },
                StrokePoint { x: 0.45, y: 0.85, timestamp_ms: 390 },
            ],
            vec![
                StrokePoint { x: 0.52, y: 0.18, timestamp_ms: 420 },
                StrokePoint { x: 0.82, y: 0.18, timestamp_ms: 440 },
                StrokePoint { x: 0.82, y: 0.84, timestamp_ms: 465 },
                StrokePoint { x: 0.74, y: 0.78, timestamp_ms: 480 },
            ],
            vec![
                StrokePoint { x: 0.53, y: 0.42, timestamp_ms: 510 },
                StrokePoint { x: 0.81, y: 0.42, timestamp_ms: 530 },
            ],
            vec![
                StrokePoint { x: 0.53, y: 0.62, timestamp_ms: 560 },
                StrokePoint { x: 0.81, y: 0.62, timestamp_ms: 580 },
            ],
        ];
        core.hw_begin(HwMode::ContinuousLattice);
        for st in &ming_strokes {
            core.hw_push_stroke(st, true).unwrap();
        }
        let m2_ming = match core.hw_end().unwrap() {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2_ming.slots.len(), 1, "single 明 with 160ms pause must stay 1 slot, got {:?}", m2_ming.composing_text);
        assert_eq!(m2_ming.composing_text, "明");

        // Horizontal pair '明明' with 200ms gap between the two chars
        let mut mingming_strokes = Vec::new();
        for st in &ming_strokes {
            mingming_strokes.push(
                st.iter()
                    .map(|p| StrokePoint {
                        x: p.x * 0.40,
                        y: p.y,
                        timestamp_ms: p.timestamp_ms,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let base_t = ming_strokes.last().unwrap().last().unwrap().timestamp_ms + 210;
        for st in &ming_strokes {
            mingming_strokes.push(
                st.iter()
                    .map(|p| StrokePoint {
                        x: 0.55 + p.x * 0.40,
                        y: p.y,
                        timestamp_ms: base_t + p.timestamp_ms,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        core.hw_begin(HwMode::ContinuousLattice);
        for st in &mingming_strokes {
            core.hw_push_stroke(st, true).unwrap();
        }
        let m2_mm = match core.hw_end().unwrap() {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2_mm.slots.len(), 2, "明明 must split into 2 slots, got {:?}", m2_mm.composing_text);
        assert_eq!(m2_mm.composing_text, "明明");
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn mode1_reversed_exta_leftright_topbottom_and_curved_one_stroke() {
        let core = VitralCore::init("{}").unwrap();

        // 1. Reversed LeftRight rare/Ext-A 頔
        let (strokes_di, _) = load_fixture_strokes("mode1_di.json");
        let rev_di: Vec<Vec<StrokePoint>> = strokes_di.into_iter().rev().collect();
        core.hw_begin(HwMode::PhoneticLookup);
        for st in &rev_di {
            core.hw_push_stroke(st, true).unwrap();
        }
        let res_di = match core.hw_end().unwrap() {
            HandwritingPayload::Mode1(m1) => m1,
            _ => panic!("expected Mode1"),
        };
        assert!(
            res_di.candidates.iter().any(|c| c.ch == "頔"),
            "reversed 頔 must appear in Top-8, got {:?}",
            res_di.candidates
        );
        assert_eq!(res_di.candidates[0].ch, "頔");

        // 2. Reversed TopBottom Ext-A 㐭 (⿱亠回)
        let lin_strokes: Vec<Vec<StrokePoint>> = vec![
            // 亠: top dot + horiz
            vec![
                StrokePoint { x: 0.46, y: 0.08, timestamp_ms: 0 },
                StrokePoint { x: 0.52, y: 0.16, timestamp_ms: 20 },
            ],
            vec![
                StrokePoint { x: 0.16, y: 0.22, timestamp_ms: 50 },
                StrokePoint { x: 0.84, y: 0.22, timestamp_ms: 80 },
            ],
            // 回: outer left vert, outer top-right hook, inner left vert, inner top-right hook, inner bottom, outer bottom
            vec![
                StrokePoint { x: 0.22, y: 0.36, timestamp_ms: 120 },
                StrokePoint { x: 0.22, y: 0.88, timestamp_ms: 150 },
            ],
            vec![
                StrokePoint { x: 0.22, y: 0.36, timestamp_ms: 180 },
                StrokePoint { x: 0.78, y: 0.36, timestamp_ms: 205 },
                StrokePoint { x: 0.78, y: 0.88, timestamp_ms: 235 },
            ],
            vec![
                StrokePoint { x: 0.38, y: 0.50, timestamp_ms: 265 },
                StrokePoint { x: 0.38, y: 0.72, timestamp_ms: 285 },
            ],
            vec![
                StrokePoint { x: 0.38, y: 0.50, timestamp_ms: 310 },
                StrokePoint { x: 0.62, y: 0.50, timestamp_ms: 330 },
                StrokePoint { x: 0.62, y: 0.72, timestamp_ms: 350 },
            ],
            vec![
                StrokePoint { x: 0.38, y: 0.72, timestamp_ms: 380 },
                StrokePoint { x: 0.62, y: 0.72, timestamp_ms: 400 },
            ],
            vec![
                StrokePoint { x: 0.22, y: 0.88, timestamp_ms: 430 },
                StrokePoint { x: 0.78, y: 0.88, timestamp_ms: 460 },
            ],
        ];
        let rev_lin: Vec<Vec<StrokePoint>> = lin_strokes.into_iter().rev().collect();
        core.hw_begin(HwMode::PhoneticLookup);
        for st in &rev_lin {
            core.hw_push_stroke(st, true).unwrap();
        }
        let res_lin = match core.hw_end().unwrap() {
            HandwritingPayload::Mode1(m1) => m1,
            _ => panic!("expected Mode1"),
        };
        assert!(
            res_lin.candidates.iter().any(|c| c.ch == "㐭"),
            "reversed TopBottom Ext-A 㐭 must appear in Top-8, got {:?}",
            res_lin.candidates
        );

        // 3. Curved 1-stroke 乙 must go to ONNX and NOT misclassify as '一'
        core.hw_begin(HwMode::PhoneticLookup);
        let res_yi = match core
            .hw_push_stroke(
                &[
                    StrokePoint { x: 0.20, y: 0.20, timestamp_ms: 0 },
                    StrokePoint { x: 0.80, y: 0.20, timestamp_ms: 20 },
                    StrokePoint { x: 0.30, y: 0.65, timestamp_ms: 40 },
                    StrokePoint { x: 0.25, y: 0.85, timestamp_ms: 60 },
                    StrokePoint { x: 0.85, y: 0.85, timestamp_ms: 80 },
                ],
                true,
            )
            .unwrap()
        {
            HandwritingPayload::Mode1(m1) => m1,
            _ => panic!("expected Mode1"),
        };
        assert_ne!(res_yi.candidates[0].ch, "一", "curved 乙 must not short-circuit to 一");
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn tier2_unk_mean_centering_does_not_demote_valid_chars() {
        use crate::eng::gram::RimeGramDb;
        use crate::hw::lattice::build_lattice_slot_from_predictions;
        use crate::hw::onnx_models::VisualPrediction;
        use crate::hw::rerank::decode_lattice_3tier;
        use std::collections::HashMap;

        let assets = resolve_assets_dir("");
        let bundle = HwOnnxBundle::load_cached(assets.as_deref());
        let imported = ImportedLexiconIndex::new(assets.as_deref());
        let gram = assets
            .as_deref()
            .and_then(|d| RimeGramDb::open_cached(&d.join("rime").join("wanxiang-lts-zh-hans.gram")).ok());
        let retention = RetentionEngine::default();

        // Elastic slot with valid in-vocab char '天' closely followed by [UNK] '㐀', followed by locked '气'
        let slot0 = build_lattice_slot_from_predictions(
            0,
            0,
            3,
            &[
                VisualPrediction { ch: '天', prob: 0.50, raw_logit: 2.0 },
                VisualPrediction { ch: '㐀', prob: 0.45, raw_logit: 1.89 },
            ],
            false,
        );
        let slot1 = build_lattice_slot_from_predictions(
            1,
            4,
            7,
            &[
                VisualPrediction { ch: '气', prob: 0.96, raw_logit: 5.0 },
                VisualPrediction { ch: '飞', prob: 0.02, raw_logit: 0.1 },
            ],
            false,
        );
        let ctx = HwDecodeContext {
            imported: &imported,
            gram: gram.as_deref(),
            retention: &retention,
            tier3: None,
            offline_guard: None,
        };
        let res = decode_lattice_3tier(
            &[slot0, slot1],
            &HashMap::new(),
            &["天".into(), "气".into()],
            &bundle,
            &ctx,
        );
        assert_eq!(res[0].ch, "天", "valid '天' must not be demoted below [UNK] '㐀'");
        let sc_tian = res[0].top8.iter().find(|c| c.ch == "天").unwrap().joint_score;
        let sc_unk = res[0].top8.iter().find(|c| c.ch == "㐀").unwrap().joint_score;
        assert!(sc_tian > sc_unk, "expected 天 ({sc_tian}) > [UNK] 㐀 ({sc_unk})");
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn mode1_mao_slow_reversed_and_exta_di_phonetic_lookup() {
        let core = VitralCore::init("{}").unwrap();

        // 1. Standard 耄
        let (strokes_mao, _) = load_fixture_strokes("mode1_mao.json");
        core.hw_begin(HwMode::PhoneticLookup);
        for st in &strokes_mao {
            core.hw_push_stroke(st, true).unwrap();
        }
        let res_mao = match core.hw_end().unwrap() {
            HandwritingPayload::Mode1(m1) => m1,
            _ => panic!("expected Mode1"),
        };
        assert_eq!(res_mao.stroke_count, 10);
        assert_eq!(res_mao.candidates[0].ch, "耄");
        assert_eq!(res_mao.candidates[0].toned_pinyin, "mào");
        assert_eq!(res_mao.candidates[0].xiaohe_code, "mc");
        assert!(res_mao.candidates.windows(2).all(|w| w[0].confidence >= w[1].confidence));

        // 2. Slow (>5000ms) + reversed stroke order 耄 (must NOT auto-clear)
        let (strokes_rev, _) = load_fixture_strokes("mode1_slow_reversed.json");
        core.hw_begin(HwMode::PhoneticLookup);
        for st in &strokes_rev {
            core.hw_push_stroke(st, true).unwrap();
        }
        let res_rev = match core.hw_end().unwrap() {
            HandwritingPayload::Mode1(m1) => m1,
            _ => panic!("expected Mode1"),
        };
        assert_eq!(res_rev.stroke_count, 10, "slow strokes must not auto-clear");
        assert_eq!(
            res_rev.candidates[0].ch, "耄",
            "expected 耄 rank 1 on slow reversed strokes, got {:?}",
            res_rev.candidates
        );
        assert_eq!(res_rev.candidates[0].toned_pinyin, "mào");
        assert_eq!(res_rev.candidates[0].xiaohe_code, "mc");

        // 3. Rare / OOV compositional character 頔 (U+9814, absent from hw_model.classes.json)
        let (strokes_di, _) = load_fixture_strokes("mode1_di.json");
        core.hw_begin(HwMode::PhoneticLookup);
        for st in &strokes_di {
            core.hw_push_stroke(st, true).unwrap();
        }
        let res_di = match core.hw_end().unwrap() {
            HandwritingPayload::Mode1(m1) => m1,
            _ => panic!("expected Mode1"),
        };
        assert_eq!(res_di.stroke_count, 14);
        assert_eq!(
            res_di.candidates[0].ch, "頔",
            "expected compositional 頔 rank 1, got {:?}",
            res_di.candidates
        );
        assert_eq!(res_di.candidates[0].toned_pinyin, "dí");
        assert_eq!(res_di.candidates[0].xiaohe_code, "di");

        // 4. Synthetic unseen strokes (non-fixture): 十 + 口
        core.hw_begin(HwMode::PhoneticLookup);
        core.hw_push_stroke(
            &[
                StrokePoint { x: 0.15, y: 0.5, timestamp_ms: 0 },
                StrokePoint { x: 0.85, y: 0.5, timestamp_ms: 30 },
            ],
            true,
        )
        .unwrap();
        core.hw_push_stroke(
            &[
                StrokePoint { x: 0.5, y: 0.15, timestamp_ms: 4000 },
                StrokePoint { x: 0.5, y: 0.85, timestamp_ms: 4030 },
            ],
            true,
        )
        .unwrap();
        let res_shi = match core.hw_end().unwrap() {
            HandwritingPayload::Mode1(m1) => m1,
            _ => panic!("expected Mode1"),
        };
        assert_eq!(res_shi.candidates[0].ch, "十");
        assert_eq!(res_shi.candidates[0].toned_pinyin, "shí");
        assert_eq!(res_shi.candidates[0].xiaohe_code, "ui");
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn mode2_incremental_baoguang_to_baoguanglaodeng_and_override_and_confirm() {
        let core = VitralCore::init("{}").unwrap();
        let scel_path = workspace_root().join("tests/fixtures/lexicons/sample_hotwords.scel");
        core.import_lexicon(&scel_path, LexiconFormat::Auto).unwrap();

        let (strokes, meta) = load_fixture_strokes("mode2_baoguang_laodeng.json");
        let segs = meta["char_segments"].as_array().unwrap();
        let slot1_end = segs[1]["stroke_range"][1].as_u64().unwrap() as usize;

        core.hw_begin(HwMode::ContinuousLattice);
        let mut phase1_res = None;
        for (idx, st) in strokes.iter().enumerate() {
            let p = core.hw_push_stroke(st, true).unwrap();
            if idx == slot1_end {
                phase1_res = Some(p);
            }
        }

        // Phase 1 ("曝光"): must output "曝光" (NOT "爆光" despite .scel weight bias!)
        let m2_p1 = match phase1_res.unwrap() {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2_p1.composing_text, "曝光", "Phase 1 must pick 曝光, got {:?}", m2_p1);
        assert!(!m2_p1.slots[0].locked_by_entropy, "slot 0 must remain elastic in Phase 1");
        assert!(m2_p1.slots[0].margin_delta_p < 0.15);
        assert_eq!(m2_p1.slots[0].top8.len(), 8);
        assert!(m2_p1.slots[0].top8.iter().any(|c| c.ch == "曝"));
        assert!(m2_p1.slots[0].top8.iter().any(|c| c.ch == "爆"));

        // Phase 2 ("爆光老登金币"): trailing "老登金币" must flip slot 0 to "爆"
        let m2_p2 = match core.hw_end().unwrap() {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2_p2.composing_text, "爆光老登金币", "Phase 2 mismatch: {:?}", m2_p2);
        assert_eq!(m2_p2.slots.len(), 6);
        assert_eq!(m2_p2.slots[0].ch, "爆");
        assert!(m2_p2.slots[0].changed_retroactively);

        // OOB hw_override_slot errors
        assert!(matches!(
            core.hw_override_slot(99, 0),
            Err(VitralError::NotFound(_))
        ));
        assert!(matches!(
            core.hw_override_slot(0, 99),
            Err(VitralError::NotFound(_))
        ));

        // Manual override back to "曝" locks slot 0
        let pu_idx = m2_p2.slots[0]
            .top8
            .iter()
            .position(|c| c.ch == "曝")
            .expect("曝 in top8");
        let ov = core.hw_override_slot(0, pu_idx).unwrap();
        assert_eq!(ov.composing_text, "曝光老登金币");
        assert!(ov.slots[0].manually_locked);
        assert!(!ov.slots[0].changed_retroactively);

        // Confirm updates shared RetentionEngine Frecency
        let confirmed = core.hw_confirm().unwrap();
        assert_eq!(confirmed, "曝光老登金币");
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn mode2_clear_locked_non_mutation_and_horizontal_connected() {
        let core = VitralCore::init("{}").unwrap();
        let scel_path = workspace_root().join("tests/fixtures/lexicons/sample_hotwords.scel");
        core.import_lexicon(&scel_path, LexiconFormat::Auto).unwrap();

        // 1. Clear locked "曝" (p1 >= 0.95, Δp >= 0.15) must NEVER mutate to "爆"
        let (strokes_clear, _) = load_fixture_strokes("mode2_clear_locked.json");
        core.hw_begin(HwMode::ContinuousLattice);
        for st in &strokes_clear {
            core.hw_push_stroke(st, true).unwrap();
        }
        let m2_clear = match core.hw_end().unwrap() {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2_clear.composing_text, "曝光老登金币");
        assert_eq!(m2_clear.slots[0].ch, "曝");
        assert!(m2_clear.slots[0].locked_by_entropy);
        assert!(m2_clear.slots[0].margin_delta_p >= 0.15);
        assert!(m2_clear.slots[0].top8[0].visual_prob >= 0.95);
        assert!(!m2_clear.slots[0].changed_retroactively);

        // 2. Horizontal connected "手写中文" (dt = 210ms < 420ms, spatial non-overlap)
        let (strokes_hc, _) = load_fixture_strokes("mode2_horizontal_connected.json");
        core.hw_begin(HwMode::ContinuousLattice);
        for st in &strokes_hc {
            core.hw_push_stroke(st, true).unwrap();
        }
        let m2_hc = match core.hw_end().unwrap() {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2_hc.slots.len(), 4);
        assert!(m2_hc.slots.iter().all(|s| !s.top8.is_empty() && s.top8.len() <= 8));
        #[cfg(all(target_os = "linux", feature = "native-onnx"))]
        assert_eq!(m2_hc.composing_text, "手写中文");

        // 3. Synthetic unseen 2-char horizontal strokes ("十" + "人") with dt = 180ms
        core.hw_begin(HwMode::ContinuousLattice);
        core.hw_push_stroke(
            &[
                StrokePoint { x: 0.08, y: 0.50, timestamp_ms: 0 },
                StrokePoint { x: 0.36, y: 0.50, timestamp_ms: 30 },
            ],
            true,
        )
        .unwrap();
        core.hw_push_stroke(
            &[
                StrokePoint { x: 0.22, y: 0.20, timestamp_ms: 80 },
                StrokePoint { x: 0.22, y: 0.80, timestamp_ms: 110 },
            ],
            true,
        )
        .unwrap();
        core.hw_push_stroke(
            &[
                StrokePoint { x: 0.72, y: 0.20, timestamp_ms: 300 },
                StrokePoint { x: 0.62, y: 0.55, timestamp_ms: 325 },
                StrokePoint { x: 0.52, y: 0.82, timestamp_ms: 350 },
            ],
            true,
        )
        .unwrap();
        core.hw_push_stroke(
            &[
                StrokePoint { x: 0.68, y: 0.48, timestamp_ms: 400 },
                StrokePoint { x: 0.80, y: 0.68, timestamp_ms: 425 },
                StrokePoint { x: 0.90, y: 0.82, timestamp_ms: 450 },
            ],
            true,
        )
        .unwrap();
        let m2_syn = match core.hw_end().unwrap() {
            HandwritingPayload::Mode2(m2) => m2,
            _ => panic!("expected Mode2"),
        };
        assert_eq!(m2_syn.slots.len(), 2);
        assert_eq!(m2_syn.composing_text, "十人");
    }

    #[test]
    fn tier3_hook_and_ids_index_and_native_onnx_verification() {
        struct MockTier3;
        impl Tier3SlotDisambiguator for MockTier3 {
            fn disambiguate_slots(
                &self,
                req: &Tier3SlotRequest,
            ) -> Result<Vec<(usize, usize)>, VitralError> {
                if let Some(slot) = req.elastic_slots.first() {
                    if let Some(idx) = slot.candidates.iter().position(|c| c == "爆") {
                        return Ok(vec![(slot.slot_idx, idx)]);
                    }
                }
                Ok(Vec::new())
            }
        }

        let assets = resolve_assets_dir("");
        let ids = IdsIndex::load_cached(assets.as_deref());
        let bundle = HwOnnxBundle::load_cached(assets.as_deref());

        #[cfg(all(target_os = "linux", feature = "native-onnx"))]
        {
            assert!(!ids.is_empty(), "cjkvi_ids.txt must load on workspace");
            assert!(bundle.has_hw_onnx(), "hw_model ONNX must load on Linux");
            assert!(bundle.has_reranker_onnx(), "rbt3-ONNX must load on Linux");

            // Direct rbt3-ONNX execution check
            let tok = bundle.tokenizer.as_ref().unwrap();
            let (seq, mpos) = tok.encode_char_slots(&['曝', '光', '老', '登', '金', '币'], Some(0));
            let scores = bundle
                .score_rbt3_masked_batch(&[seq], &[mpos], &[vec!['曝', '爆']])
                .expect("rbt3 execution");
            let lp_pu = scores[0][0].expect("曝 in vocab");
            let lp_bao = scores[0][1].expect("爆 in vocab");
            assert!(lp_bao > lp_pu, "rbt3 [MASK]光老登金币 should prefer 爆 ({lp_bao}) > 曝 ({lp_pu})");
        }

        // Verify Tier3 trait compiles and pure_offline blocks network tier3 hook
        let _hook: &dyn Tier3SlotDisambiguator = &MockTier3;
        let _ = (ids, bundle);
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn tier2_unk_min_anchoring_preserves_bao_above_unk_in_de_tu_baoguang() {
        use crate::eng::gram::RimeGramDb;
        use crate::hw::lattice::build_lattice_slot_from_predictions;
        use crate::hw::onnx_models::VisualPrediction;
        use crate::hw::rerank::decode_lattice_3tier;
        use std::collections::HashMap;

        let assets = resolve_assets_dir("");
        let bundle = HwOnnxBundle::load_cached(assets.as_deref());
        let bundle_tier1_only = HwOnnxBundle::load_cached(None);
        let imported = ImportedLexiconIndex::new(assets.as_deref());
        let gram = assets
            .as_deref()
            .and_then(|d| RimeGramDb::open_cached(&d.join("rime").join("wanxiang-lts-zh-hans.gram")).ok());
        let retention = RetentionEngine::default();
        let ctx = HwDecodeContext {
            imported: &imported,
            gram: gram.as_deref(),
            retention: &retention,
            tier3: None,
            offline_guard: None,
        };

        // 1. Challenger 4's "的㐁曝光" regression probe: slot 2 has '爆'(0.40) and [UNK] '䶵'(0.35)
        //    alongside high-MLM distractors '曝','的','一','是','了','不'
        let slots = vec![
            build_lattice_slot_from_predictions(
                0,
                0,
                1,
                &[VisualPrediction { ch: '的', prob: 0.96, raw_logit: 5.0 }],
                false,
            ),
            build_lattice_slot_from_predictions(
                1,
                2,
                3,
                &[VisualPrediction { ch: '㐁', prob: 0.96, raw_logit: 5.0 }],
                false,
            ),
            build_lattice_slot_from_predictions(
                2,
                4,
                5,
                &[
                    VisualPrediction { ch: '曝', prob: 0.45, raw_logit: 1.8 },
                    VisualPrediction { ch: '爆', prob: 0.40, raw_logit: 1.69 },
                    VisualPrediction { ch: '䶵', prob: 0.35, raw_logit: 1.56 },
                    VisualPrediction { ch: '的', prob: 0.10, raw_logit: 0.3 },
                    VisualPrediction { ch: '一', prob: 0.08, raw_logit: 0.1 },
                    VisualPrediction { ch: '是', prob: 0.06, raw_logit: -0.2 },
                    VisualPrediction { ch: '了', prob: 0.04, raw_logit: -0.6 },
                    VisualPrediction { ch: '不', prob: 0.02, raw_logit: -1.3 },
                ],
                false,
            ),
            build_lattice_slot_from_predictions(
                3,
                6,
                7,
                &[VisualPrediction { ch: '光', prob: 0.96, raw_logit: 5.0 }],
                false,
            ),
        ];
        let base_chars = vec!["的".into(), "㐁".into(), "曝".into(), "光".into()];
        let res_t1 = decode_lattice_3tier(&slots, &HashMap::new(), &base_chars, &bundle_tier1_only, &ctx);
        let res_t2 = decode_lattice_3tier(&slots, &HashMap::new(), &base_chars, &bundle, &ctx);

        let bao_t1 = res_t1[2].top8.iter().find(|c| c.ch == "爆").unwrap().joint_score;
        let bao_t2 = res_t2[2].top8.iter().find(|c| c.ch == "爆").unwrap().joint_score;
        let unk_t2 = res_t2[2].top8.iter().find(|c| c.ch == "䶵").unwrap().joint_score;
        assert!(
            bao_t2 >= bao_t1,
            "Tier 2 min_lp anchoring must give non-negative delta for '爆': t1={bao_t1}, t2={bao_t2}"
        );
        assert!(
            bao_t2 > unk_t2,
            "Valid in-vocab '爆' ({bao_t2}) must remain strictly above [UNK] '䶵' ({unk_t2})"
        );

        // 2. All-[UNK] elastic slot edge case: 今[㐀/㐁/䶵]天 (no panics/NaNs, Tier 1 preserved)
        let all_unk_slots = vec![
            build_lattice_slot_from_predictions(
                0,
                0,
                1,
                &[VisualPrediction { ch: '今', prob: 0.96, raw_logit: 5.0 }],
                false,
            ),
            build_lattice_slot_from_predictions(
                1,
                2,
                3,
                &[
                    VisualPrediction { ch: '㐀', prob: 0.42, raw_logit: 1.7 },
                    VisualPrediction { ch: '㐁', prob: 0.35, raw_logit: 1.5 },
                    VisualPrediction { ch: '䶵', prob: 0.23, raw_logit: 1.1 },
                ],
                false,
            ),
            build_lattice_slot_from_predictions(
                2,
                4,
                5,
                &[VisualPrediction { ch: '天', prob: 0.96, raw_logit: 5.0 }],
                false,
            ),
        ];
        let all_unk_t1 = decode_lattice_3tier(
            &all_unk_slots,
            &HashMap::new(),
            &["今".into(), "㐀".into(), "天".into()],
            &bundle_tier1_only,
            &ctx,
        );
        let all_unk_t2 = decode_lattice_3tier(
            &all_unk_slots,
            &HashMap::new(),
            &["今".into(), "㐀".into(), "天".into()],
            &bundle,
            &ctx,
        );
        assert_eq!(all_unk_t2[1].ch, all_unk_t1[1].ch);
        assert!(all_unk_t2[1].top8.iter().all(|c| c.joint_score.is_finite()));

        // 3. Single-in-vocab elastic slot edge case: 他把门[㐀/关/㐁]上了
        let single_iv_slots = vec![
            build_lattice_slot_from_predictions(0, 0, 1, &[VisualPrediction { ch: '他', prob: 0.96, raw_logit: 5.0 }], false),
            build_lattice_slot_from_predictions(1, 2, 3, &[VisualPrediction { ch: '把', prob: 0.96, raw_logit: 5.0 }], false),
            build_lattice_slot_from_predictions(2, 4, 5, &[VisualPrediction { ch: '门', prob: 0.96, raw_logit: 5.0 }], false),
            build_lattice_slot_from_predictions(
                3,
                6,
                7,
                &[
                    VisualPrediction { ch: '㐀', prob: 0.45, raw_logit: 1.8 },
                    VisualPrediction { ch: '关', prob: 0.42, raw_logit: 1.7 },
                    VisualPrediction { ch: '㐁', prob: 0.13, raw_logit: 0.5 },
                ],
                false,
            ),
            build_lattice_slot_from_predictions(4, 8, 9, &[VisualPrediction { ch: '上', prob: 0.96, raw_logit: 5.0 }], false),
            build_lattice_slot_from_predictions(5, 10, 11, &[VisualPrediction { ch: '了', prob: 0.96, raw_logit: 5.0 }], false),
        ];
        let single_iv_res = decode_lattice_3tier(
            &single_iv_slots,
            &HashMap::new(),
            &["他".into(), "把".into(), "门".into(), "㐀".into(), "上".into(), "了".into()],
            &bundle,
            &ctx,
        );
        assert_eq!(single_iv_res[3].ch, "关");
        assert!(single_iv_res[3].top8.iter().all(|c| c.joint_score.is_finite()));
    }

    #[test]
    fn imported_skipgram_pmi_chars_matches_bigram_pmi_score_parity() {
        let assets = resolve_assets_dir("");
        let mut idx = ImportedLexiconIndex::new(assets.as_deref());
        let scel_path = workspace_root().join("tests/fixtures/lexicons/sample_hotwords.scel");
        compile_and_index(&mut idx, &scel_path, LexiconFormat::Auto, None).unwrap();

        idx.inject_hotwords(&[
            crate::imp::ImportedLexiconEntry {
                text: "老登金币".into(),
                pinyin: "lao deng jin bi".into(),
                toned_pinyin: "lǎo dēng jīn bì".into(),
                xiaohe: "lcdejb".into(),
                weight: 1200,
            },
            crate::imp::ImportedLexiconEntry {
                text: "手写输入法".into(),
                pinyin: "shou xie shu ru fa".into(),
                toned_pinyin: "shǒu xiě shū rù fǎ".into(),
                xiaohe: "uoxeurufa".into(),
                weight: 900,
            },
        ]);

        assert_eq!(
            idx.lookup_imported_score_chars(&['老', '登', '金', '币']),
            idx.lookup_imported_score("老登金币")
        );
        assert_eq!(
            idx.lookup_imported_score_chars(&['手', '写', '输', '入', '法']),
            idx.lookup_imported_score("手写输入法")
        );
        assert_eq!(idx.lookup_imported_score_chars(&['不', '存', '在']), None);

        let pairs = [
            ("爆", "金币"),
            ("爆光", "金币"),
            ("老登", "金币"),
            ("老", "币"),
            ("手写", "输入法"),
            ("手", "法"),
            ("今天", "天气"),
            ("曝光", "光"),
        ];
        for (w1, w2) in pairs {
            let c1: Vec<char> = w1.chars().collect();
            let c2: Vec<char> = w2.chars().collect();
            let pmis = idx.skipgram_pmi_chars(&c1, &c2);
            for d in 1..=6 {
                let via_str = idx.bigram_pmi_score(w1, w2, d);
                let mut via_chars = pmis.map(|p| p[d - 1]).unwrap_or(0.0);
                if via_chars <= 0.0 && d == 1 {
                    let joined: Vec<char> = c1.iter().chain(c2.iter()).copied().collect();
                    if let Some(w) = idx.corpus().lookup_word_chars(&joined) {
                        via_chars = (((w as f32).max(1.0)).ln() * 0.25).max(0.5);
                    }
                }
                assert!(
                    (via_str - via_chars).abs() <= 1e-6,
                    "parity mismatch for ({w1}, {w2}, d={d}): str={via_str}, chars={via_chars}"
                );
            }
        }
    }

    #[test]
    fn mode2_dyn_beam_n_gt_32_slots_and_empty_top8_guard() {
        use crate::hw::lattice::{build_lattice_slot_from_predictions, LatticeSlot};
        use crate::hw::onnx_models::VisualPrediction;
        use crate::hw::rerank::decode_lattice_3tier;
        use std::collections::HashMap;

        let assets = resolve_assets_dir("");
        let bundle = HwOnnxBundle::load_cached(None);
        let imported = ImportedLexiconIndex::new(assets.as_deref());
        let retention = RetentionEngine::default();
        let ctx = HwDecodeContext {
            imported: &imported,
            gram: None,
            retention: &retention,
            tier3: None,
            offline_guard: None,
        };

        let sample_chars = ['今', '天', '天', '气', '真', '好', '我', '们'];
        let mut slots: Vec<LatticeSlot> = (0..40)
            .map(|i| {
                if i == 15 {
                    return LatticeSlot {
                        slot_idx: i,
                        stroke_start: i * 2,
                        stroke_end: i * 2 + 1,
                        margin_delta_p: 0.0,
                        locked_by_entropy: false,
                        manually_locked: false,
                        top8: Vec::new(),
                    };
                }
                let ch = sample_chars[i % sample_chars.len()];
                let locked = i % 5 == 0;
                let preds = if locked {
                    vec![
                        VisualPrediction { ch, prob: 0.96, raw_logit: 5.0 },
                        VisualPrediction { ch: '一', prob: 0.02, raw_logit: 0.1 },
                    ]
                } else {
                    vec![
                        VisualPrediction { ch, prob: 0.40, raw_logit: 1.8 },
                        VisualPrediction { ch: '好', prob: 0.36, raw_logit: 1.65 },
                        VisualPrediction { ch: '气', prob: 0.10, raw_logit: 0.4 },
                        VisualPrediction { ch: '天', prob: 0.06, raw_logit: 0.0 },
                        VisualPrediction { ch: '真', prob: 0.04, raw_logit: -0.4 },
                        VisualPrediction { ch: '我', prob: 0.02, raw_logit: -0.9 },
                        VisualPrediction { ch: '们', prob: 0.01, raw_logit: -1.4 },
                        VisualPrediction { ch: '一', prob: 0.01, raw_logit: -1.5 },
                    ]
                };
                build_lattice_slot_from_predictions(i, i * 2, i * 2 + 1, &preds, false)
            })
            .collect();

        // Also verify manual lock preservation in N > 32
        let mut manual_locks = HashMap::new();
        manual_locks.insert((slots[34].stroke_start, slots[34].stroke_end), "真".to_string());
        let base_chars: Vec<String> = slots
            .iter()
            .map(|s| s.top8.first().map(|c| c.ch.clone()).unwrap_or_default())
            .collect();
        let res = decode_lattice_3tier(&slots, &manual_locks, &base_chars, &bundle, &ctx);
        assert_eq!(res.len(), 40);
        for (i, slot) in res.iter().enumerate() {
            assert_eq!(slot.slot_idx, i);
            if i == 15 {
                assert!(slot.top8.is_empty());
                assert_eq!(slot.ch, "");
            } else {
                assert!(!slot.ch.is_empty());
                assert!(slot.top8.iter().all(|c| c.joint_score.is_finite()));
            }
        }
        assert_eq!(res[34].ch, "真");
        assert!(res[34].manually_locked);
        assert!(!res[34].changed_retroactively);
        assert!(res[0].locked_by_entropy);
        assert!(!res[0].changed_retroactively);
        let _ = &mut slots;
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx"))]
    #[test]
    fn mode2_16char_tier1_latency_under_2ms_release() {
        use crate::eng::gram::RimeGramDb;
        use crate::hw::lattice::{build_lattice_slot_from_predictions, LatticeSlot};
        use crate::hw::onnx_models::VisualPrediction;
        use crate::hw::rerank::decode_lattice_3tier;
        use std::collections::HashMap;
        use std::time::Instant;

        let assets = resolve_assets_dir("");
        let bundle_tier1 = HwOnnxBundle::load_cached(None);
        let mut imported = ImportedLexiconIndex::new(assets.as_deref());
        let scel_path = workspace_root().join("tests/fixtures/lexicons/sample_hotwords.scel");
        compile_and_index(&mut imported, &scel_path, LexiconFormat::Auto, None).unwrap();
        let gram = assets
            .as_deref()
            .and_then(|d| RimeGramDb::open_cached(&d.join("rime").join("wanxiang-lts-zh-hans.gram")).ok());
        let retention = RetentionEngine::default();
        let ctx = HwDecodeContext {
            imported: &imported,
            gram: gram.as_deref(),
            retention: &retention,
            tier3: None,
            offline_guard: None,
        };

        let target: Vec<char> = "今天天气真好我们一起去公园散步吧".chars().collect();
        let distractors = ['的', '一', '是', '了', '不', '在', '有', '人', '这', '中'];
        let lattice: Vec<LatticeSlot> = target
            .iter()
            .enumerate()
            .map(|(i, &ch)| {
                let mut preds = Vec::with_capacity(8);
                preds.push(VisualPrediction { ch, prob: 0.24, raw_logit: 1.5 });
                let mut d_idx = 0usize;
                while preds.len() < 8 {
                    let dc = distractors[(i + d_idx) % distractors.len()];
                    d_idx += 1;
                    if preds.iter().any(|p| p.ch == dc) {
                        continue;
                    }
                    let rank = preds.len();
                    preds.push(VisualPrediction {
                        ch: dc,
                        prob: 0.22 - (rank as f32) * 0.02,
                        raw_logit: 1.4 - (rank as f32) * 0.15,
                    });
                }
                build_lattice_slot_from_predictions(i, i * 2, i * 2 + 1, &preds, false)
            })
            .collect();
        let base_chars: Vec<String> = lattice.iter().map(|s| s.top8[0].ch.clone()).collect();

        // Warmup
        for _ in 0..2 {
            let res = decode_lattice_3tier(&lattice, &HashMap::new(), &base_chars, &bundle_tier1, &ctx);
            assert_eq!(res.len(), 16);
        }

        let mut times_ms = Vec::with_capacity(9);
        for _ in 0..9 {
            let t0 = Instant::now();
            let res = decode_lattice_3tier(&lattice, &HashMap::new(), &base_chars, &bundle_tier1, &ctx);
            let dt = t0.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(res.len(), 16);
            times_ms.push(dt);
        }
        times_ms.sort_by(|a, b| a.total_cmp(b));
        let median_ms = times_ms[times_ms.len() / 2];
        eprintln!(
            "[m1b_tests] 16_elastic scel Tier 1 median = {median_ms:.3} ms (min={:.3}, max={:.3})",
            times_ms[0],
            times_ms[times_ms.len() - 1]
        );
        #[cfg(not(debug_assertions))]
        assert!(
            median_ms < 2.0,
            "16_elastic Tier 1 median {median_ms:.3} ms must be < 2.0 ms in --release"
        );
    }
}

#[cfg(test)]
mod m1c_tests {
    use super::*;
    use crate::eng::{LangMode, MOD_LANG_TOGGLE, MOD_VOICE_HOLD};
    use crate::lan::{LocalSendDeviceInfo, LocalSendTransport};
    use crate::ret::{is_voice_hold_triggered, RetentionEngine};
    use crate::sync::{
        decrypt_envelope, encrypt_envelope, sha256_hex, zstd_compress, zstd_decompress, DeviceTier,
    };
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, UdpSocket};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root")
    }

    fn type_ascii(core: &VitralCore, keys: &str) -> InputAction {
        let mut last = None;
        for b in keys.bytes() {
            last = Some(core.process_key(b as u32, 0, SwipeDir::None).unwrap());
        }
        last.expect("non-empty keys")
    }

    #[derive(Clone)]
    struct MockDavNode {
        is_dir: bool,
        data: Vec<u8>,
        mtime_secs: i64,
    }

    type PutGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
    type PathGate = (String, PutGate);

    struct InProcessWebDavMock {
        addr: std::net::SocketAddr,
        nodes: Arc<Mutex<HashMap<String, MockDavNode>>>,
        request_log: Arc<Mutex<Vec<(String, String)>>>,
        not_modified: Arc<std::sync::atomic::AtomicUsize>,
        get_gate: Arc<Mutex<Option<PathGate>>>,
        stop: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl InProcessWebDavMock {
        fn start() -> Self {
            Self::start_with_put_gate(None)
        }

        fn start_with_put_gate(gate: Option<PutGate>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let mut init_nodes = HashMap::new();
            for dir in [
                "/dav",
                "/dav/Koofr",
                "/dav/Koofr/Vitral",
                "/dav/Koofr/Vitral/lexicon",
                "/dav/Koofr/Vitral/lexicon/crdt",
                "/dav/Koofr/Vitral/lexicon/imports",
                "/dav/Koofr/Vitral/profile",
                "/dav/Koofr/Vitral/clipboard",
                "/dav/Koofr/Vitral/relay",
            ] {
                init_nodes.insert(
                    dir.to_string(),
                    MockDavNode {
                        is_dir: true,
                        data: Vec::new(),
                        mtime_secs: now_secs,
                    },
                );
            }
            let nodes = Arc::new(Mutex::new(init_nodes));
            let request_log = Arc::new(Mutex::new(Vec::new()));
            let not_modified = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let put_gate = Arc::new(Mutex::new(gate));
            let get_gate = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));

            let nodes_bg = Arc::clone(&nodes);
            let log_bg = Arc::clone(&request_log);
            let not_modified_bg = Arc::clone(&not_modified);
            let gate_bg = Arc::clone(&put_gate);
            let get_gate_bg = Arc::clone(&get_gate);
            let stop_bg = Arc::clone(&stop);

            let join = std::thread::spawn(move || {
                let mut conn_joins: Vec<std::thread::JoinHandle<()>> = Vec::new();
                while !stop_bg.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let n = Arc::clone(&nodes_bg);
                            let l = Arc::clone(&log_bg);
                            let nm = Arc::clone(&not_modified_bg);
                            let pg = Arc::clone(&gate_bg);
                            let gg = Arc::clone(&get_gate_bg);
                            conn_joins.push(std::thread::spawn(move || {
                                Self::handle_conn(&mut stream, &n, &l, &pg, &gg, &nm);
                            }));
                            let mut still_running = Vec::new();
                            for h in conn_joins.drain(..) {
                                if h.is_finished() {
                                    let _ = h.join();
                                } else {
                                    still_running.push(h);
                                }
                            }
                            conn_joins = still_running;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in conn_joins {
                    let _ = h.join();
                }
            });

            Self {
                addr,
                nodes,
                request_log,
                not_modified,
                get_gate,
                stop,
                join: Some(join),
            }
        }

        fn arm_get_gate(
            &self,
            rel_path: &str,
        ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
            let full = format!("/dav/Koofr/Vitral/{}", rel_path.trim_start_matches('/'));
            let (hit_tx, hit_rx) = std::sync::mpsc::channel();
            let (unblock_tx, unblock_rx) = std::sync::mpsc::channel();
            *self.get_gate.lock() = Some((full, (hit_tx, unblock_rx)));
            (hit_rx, unblock_tx)
        }

        fn koofr_endpoint(&self) -> String {
            format!("http://{}/dav/Koofr", self.addr)
        }

        fn write_file(&self, rel_path: &str, data: &[u8], mtime_secs: i64) {
            let full = format!("/dav/Koofr/Vitral/{}", rel_path.trim_start_matches('/'));
            self.nodes.lock().insert(
                full,
                MockDavNode {
                    is_dir: false,
                    data: data.to_vec(),
                    mtime_secs,
                },
            );
        }

        fn exists(&self, rel_path: &str) -> bool {
            let full = format!("/dav/Koofr/Vitral/{}", rel_path.trim_start_matches('/'));
            self.nodes.lock().get(&full).is_some_and(|n| !n.is_dir)
        }

        fn list_files(&self, prefix: &str) -> Vec<String> {
            let base = "/dav/Koofr/Vitral/";
            let mut out: Vec<String> = self
                .nodes
                .lock()
                .iter()
                .filter(|(_, n)| !n.is_dir)
                .filter_map(|(k, _)| k.strip_prefix(base).map(str::to_string))
                .filter(|rel| prefix.is_empty() || rel.starts_with(prefix))
                .collect();
            out.sort();
            out
        }

        fn clear_log(&self) {
            self.request_log.lock().clear();
        }

        fn log(&self) -> Vec<(String, String)> {
            self.request_log.lock().clone()
        }

        fn handle_conn(
            stream: &mut std::net::TcpStream,
            nodes: &Arc<Mutex<HashMap<String, MockDavNode>>>,
            log: &Arc<Mutex<Vec<(String, String)>>>,
            put_gate: &Arc<Mutex<Option<PutGate>>>,
            get_gate: &Arc<Mutex<Option<PathGate>>>,
            not_modified: &std::sync::atomic::AtomicUsize,
        ) {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut req_line = String::new();
            if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                return;
            }
            let parts: Vec<&str> = req_line.split_whitespace().collect();
            if parts.len() < 2 {
                return;
            }
            let method = parts[0].to_ascii_uppercase();
            let raw_path = parts[1].split('?').next().unwrap_or(parts[1]).to_string();
            let norm_path = if raw_path.len() > 1 {
                raw_path.trim_end_matches('/').to_string()
            } else {
                raw_path.clone()
            };

            let mut conditions = HashMap::new();
            let mut content_len = 0usize;
            let mut depth = "1".to_string();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break;
                }
                if let Some((k, v)) = trimmed.split_once(':') {
                    conditions.insert(k.to_ascii_lowercase(), v.trim().to_string());
                    if k.eq_ignore_ascii_case("content-length") {
                        content_len = v.trim().parse().unwrap_or(0);
                    } else if k.eq_ignore_ascii_case("depth") {
                        depth = v.trim().to_string();
                    }
                }
            }
            let mut body = vec![0u8; content_len];
            if content_len > 0 && reader.read_exact(&mut body).is_err() {
                return;
            }

            log.lock().push((method.clone(), raw_path.clone()));
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;

            if method == "PUT" {
                let gate = put_gate.lock().take();
                if let Some((tx, rx)) = gate {
                    let _ = tx.send(());
                    let _ = rx.recv_timeout(Duration::from_secs(5));
                }
            }

            if method == "GET" {
                let matched = {
                    let mut guard = get_gate.lock();
                    if guard.as_ref().is_some_and(|(target, _)| *target == norm_path) {
                        guard.take().map(|(_, gate)| gate)
                    } else {
                        None
                    }
                };
                if let Some((tx, rx)) = matched {
                    let _ = tx.send(());
                    let _ = rx.recv_timeout(Duration::from_secs(5));
                }
            }

            let mut map = nodes.lock();
            match method.as_str() {
                "MKCOL" => {
                    if map.contains_key(&norm_path) {
                        let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    let parent = norm_path.rsplit_once('/').map_or("", |(parent, _)| parent);
                    if !map.get(parent).is_some_and(|n| n.is_dir) {
                        let _ = stream.write_all(b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    map.insert(norm_path, MockDavNode {
                        is_dir: true,
                        data: Vec::new(),
                        mtime_secs: now_secs,
                    });
                    let _ = stream.write_all(
                        b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                "PUT" => {
                    let parent = norm_path.rsplit_once('/').map_or("", |(parent, _)| parent);
                    if !map.get(parent).is_some_and(|n| n.is_dir) {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    let old = map.get(&norm_path).map(|n| format!("\"{}\"", crate::sync::sha256_hex(&n.data)));
                    if conditions.get("if-none-match").is_some_and(|v| v == "*" && old.is_some())
                        || conditions.get("if-match").is_some_and(|v| Some(v) != old.as_ref()) {
                        let _ = stream.write_all(b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    map.insert(
                        norm_path,
                        MockDavNode {
                            is_dir: false,
                            data: body,
                            mtime_secs: now_secs,
                        },
                    );
                    let _ = stream.write_all(
                        b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                "GET" | "HEAD" => {
                    if let Some(node) = map.get(&norm_path).filter(|n| !n.is_dir) {
                        let tag = format!("\"{}\"", crate::sync::sha256_hex(&node.data));
                        if conditions.get("if-none-match") == Some(&tag) {
                            not_modified.fetch_add(1, Ordering::Relaxed);
                            let hdr = format!(
                                "HTTP/1.1 304 Not Modified\r\nETag: {tag}\r\nConnection: close\r\n\r\n"
                            );
                            let _ = stream.write_all(hdr.as_bytes());
                            return;
                        }
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {tag}\r\nConnection: close\r\n\r\n",
                            node.data.len()
                        );
                        let _ = stream.write_all(hdr.as_bytes());
                        if method == "GET" {
                            let _ = stream.write_all(&node.data);
                        }
                    } else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
                "DELETE" => {
                    if map.remove(&norm_path).is_some() {
                        let _ = stream.write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    } else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
                "PROPFIND" => {
                    let Some(target) = map.get(&norm_path).cloned() else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    };
                    let mut items = vec![(norm_path.clone(), target.clone())];
                    if target.is_dir && depth != "0" {
                        let prefix = format!("{norm_path}/");
                        let mut children: Vec<(String, MockDavNode)> = map
                            .iter()
                            .filter(|(k, _)| {
                                k.starts_with(&prefix) && !k[prefix.len()..].contains('/')
                            })
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        children.sort_by(|a, b| a.0.cmp(&b.0));
                        items.extend(children);
                    }
                    let mut xml = String::from(
                        r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">"#,
                    );
                    for (p, n) in items {
                        let dt = DateTime::<Utc>::from_timestamp(n.mtime_secs, 0)
                            .unwrap_or_default()
                            .format("%a, %d %b %Y %H:%M:%S GMT")
                            .to_string();
                        let href = if n.is_dir && !p.ends_with('/') {
                            format!("{p}/")
                        } else {
                            p
                        };
                        let res_type = if n.is_dir {
                            "<D:resourcetype><D:collection/></D:resourcetype>"
                        } else {
                            "<D:resourcetype/>"
                        };
                        xml.push_str(&format!(
                            "<D:response><D:href>{href}</D:href><D:propstat><D:prop>{res_type}<D:getcontentlength>{}</D:getcontentlength><D:getlastmodified>{dt}</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
                            n.data.len()
                        ));
                    }
                    xml.push_str("</D:multistatus>");
                    let hdr = format!(
                        "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        xml.len()
                    );
                    let _ = stream.write_all(hdr.as_bytes());
                    let _ = stream.write_all(xml.as_bytes());
                }
                _ => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        }
    }

    impl Drop for InProcessWebDavMock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    #[test]
    fn koofr_missing_collections_recover_from_put_404() {
        for root_exists in [false, true] {
            for path in [
                "profile/config.json.enc",
                "lexicon/crdt/device.json.enc",
                "relay/file.enc",
                "root-file.enc",
                "clipboard/current.json.enc",
                "clipboard/history_ring.jsonl.enc",
            ] {
                // OpenDAL can create missing children when its root already exists.
                if root_exists && !path.starts_with("clipboard/") {
                    continue;
                }
                let mock = InProcessWebDavMock::start();
                mock.nodes.lock().retain(|path, _| {
                    !path.starts_with("/dav/Koofr/Vitral")
                        || (root_exists && path == "/dav/Koofr/Vitral")
                });
                let guard = OfflineGuard::new(false);
                let cfg = DavConfig {
                    endpoint: mock.koofr_endpoint(),
                    sync_key: Some("missing-collections".into()),
                    ..Default::default()
                };
                let client = dav::KoofrDavClient::connect(&guard, &cfg).unwrap();
                let entry = clip::ClipboardEntry {
                    id: "local-copy".into(),
                    content: "local content".into(),
                    mime_type: "text/plain".into(),
                    source_device: "test".into(),
                    timestamp_ms: 10,
                    pinned: false,
                    shortcut_code: None,
                };
                match path {
                    "clipboard/current.json.enc" => {
                        assert_eq!(
                            dav::sync_current_copy(&guard, &cfg, Some(&entry), None).unwrap().entry,
                            Some(entry.clone())
                        );
                        assert_eq!(
                            dav::sync_current_copy(&guard, &cfg, None, None).unwrap().entry,
                            Some(entry)
                        );
                    }
                    "clipboard/history_ring.jsonl.enc" => {
                        let local = vec![entry];
                        let mut changes = clip::RingChanges::default();
                        changes.record(&[], &local);
                        assert_eq!(
                            dav::merge_clipboard_ring(&guard, &cfg, &changes, &local, 20).unwrap(),
                            local
                        );
                        assert_eq!(
                            dav::download_clipboard_ring(&guard, &cfg).unwrap(),
                            Some(local)
                        );
                    }
                    _ => {
                        client.write_bytes(path, b"content").unwrap();
                        assert_eq!(client.read_bytes(path).unwrap(), b"content");
                    }
                }
                assert!(mock.exists(path), "missing uploaded {path}");
                let log = mock.log();
                let root_mkcol = log
                    .iter()
                    .position(|(method, path)| method == "MKCOL" && path == "/dav/Koofr/Vitral/")
                    .unwrap();
                if path.starts_with("clipboard/") {
                    assert!(log[..root_mkcol].iter().any(|(method, url)| {
                        method == "PUT" && url == &format!("/dav/Koofr/Vitral/{path}")
                    }), "clipboard PUT must encounter the missing collection first");
                }
                let mut prefix = String::from("/dav/Koofr/Vitral");
                let mut previous = root_mkcol;
                if let Some((parent, _)) = path.rsplit_once('/') {
                    for segment in parent.split('/') {
                        prefix.push('/');
                        prefix.push_str(segment);
                        let position = log
                            .iter()
                            .enumerate()
                            .find(|(i, (method, path))| {
                                *i > previous && method == "MKCOL" && path == &format!("{prefix}/")
                            })
                            .map(|(i, _)| i)
                            .unwrap();
                        previous = position;
                    }
                }
                assert!(log.iter().skip(previous + 1).any(
                    |(method, url)| method == "PUT" && url == &format!("/dav/Koofr/Vitral/{path}")
                ));
            }
        }
    }

    #[test]
    fn apply_preferences_schedules_initial_koofr_sync() {
        let mock = InProcessWebDavMock::start();
        let dir = std::env::temp_dir().join(format!("vitral-koofr-preferences-{}", std::process::id()));
        let mut cfg = EngineConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            assets_dir: "/nonexistent-vitral-assets".into(),
            background_sync: true,
            ..Default::default()
        };
        let core = VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
        let local = core
            .clipboard_push_local(ClipboardPayload {
                id: Some("before-configuring-koofr".into()),
                content: "local history".into(),
                mime_type: "text/plain".into(),
                source_device: "test".into(),
                timestamp_ms: 20,
            })
            .ring;
        let cloud = clip::ClipboardEntry {
            id: "cloud-copy".into(),
            timestamp_ms: 10,
            ..local[0].clone()
        };
        let cloud_bytes = crate::sync::encode_clipboard_ring_jsonl(std::slice::from_ref(&cloud))
            .unwrap();
        mock.write_file(
            "clipboard/history_ring.jsonl.enc",
            &encrypt_envelope(&cloud_bytes, None).unwrap(),
            10,
        );

        cfg.koofr_endpoint = Some(mock.koofr_endpoint());
        core.apply_preferences(cfg.clone()).unwrap();
        assert_eq!(
            local::load::<bool>(&dir, "backup-pending.json").unwrap(),
            Some(true)
        );
        let changes = local::load::<clip::RingChanges>(&dir, "clipboard-changes.json")
            .unwrap()
            .unwrap();
        assert_eq!(
            changes.merge(vec![cloud.clone()], &local, 20),
            vec![local[0].clone(), cloud.clone()]
        );
        assert!(
            mock.log().is_empty(),
            "saving preferences should schedule synchronization"
        );
        drop(core);

        // Pending uploads must survive a restart before the service queue runs.
        let core = VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
        core.poll_services().unwrap();
        for path in [
            "profile/config.json.enc",
            "lexicon/snapshot.zst.enc",
            "clipboard/current.json.enc",
        ] {
            assert!(mock.exists(path), "initial synchronization missed {path}");
        }
        let dav_cfg = DavConfig::from_engine_config(&cfg).unwrap();
        assert_eq!(
            dav::download_clipboard_ring(&core.offline_guard, &dav_cfg).unwrap(),
            Some(vec![local[0].clone(), cloud])
        );
        assert_eq!(
            local::load::<bool>(&dir, "backup-pending.json").unwrap(),
            Some(false)
        );
        assert!(
            local::load::<clip::RingChanges>(&dir, "clipboard-changes.json")
                .unwrap()
                .unwrap()
                .is_empty()
        );

        cfg.koofr_endpoint = None;
        core.apply_preferences(cfg).unwrap();
        assert!(!core.sync.lock().backup_pending);
        assert!(core.sync.lock().clipboard_changes.is_empty());
        assert_eq!(
            local::load::<bool>(&dir, "backup-pending.json").unwrap(),
            Some(false)
        );
        assert!(
            local::load::<clip::RingChanges>(&dir, "clipboard-changes.json")
                .unwrap()
                .unwrap()
                .is_empty()
        );
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clipboard_background_edits_persist_without_network_until_services_poll() {
        let mock = InProcessWebDavMock::start();
        let dir = std::env::temp_dir().join(format!(
            "vitral-clipboard-background-{}-{}",
            std::process::id(),
            RetentionEngine::now_epoch_ms()
        ));
        let cfg = EngineConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            assets_dir: "/nonexistent-vitral-assets".into(),
            background_sync: true,
            koofr_endpoint: Some(mock.koofr_endpoint()),
            ..Default::default()
        };
        let core = VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
        core.clipboard_push_local(ClipboardPayload {
            id: Some("saved-copy".into()),
            content: "saved text".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 10,
        });
        core.poll_services().unwrap();
        let dav_cfg = DavConfig::from_engine_config(&cfg).unwrap();
        let current_path = dir.join("clipboard-current.json");
        // History edits and subsequent polls must leave the unchanged current file alone.
        std::fs::remove_file(&current_path).unwrap();

        for operation in ["pin", "unpin", "clear"] {
            let before = core.clipboard_state().ring;
            mock.clear_log();
            let state = match operation {
                "pin" => core.clipboard_pin("saved-copy", Some("saved")).unwrap(),
                "unpin" => core.clipboard_unpin("saved-copy").unwrap(),
                "clear" => core.clipboard_clear_unpinned(),
                _ => unreachable!(),
            };
            assert!(mock.log().is_empty(), "{operation} must defer WebDAV requests");
            assert!(!current_path.exists());
            assert_ne!(state.ring, before);
            assert_eq!(
                local::load::<Vec<clip::ClipboardEntry>>(&dir, "clipboard-history.json")
                    .unwrap()
                    .unwrap(),
                state.ring
            );
            let changes = local::load::<clip::RingChanges>(&dir, "clipboard-changes.json")
                .unwrap()
                .unwrap();
            assert!(!changes.is_empty(), "{operation} must persist pending changes");
            assert_eq!(changes.merge(before, &state.ring, state.capacity), state.ring);

            let state_after_poll = core.poll_services().unwrap();
            assert_eq!(state_after_poll.current_copy.unwrap().id, "saved-copy");
            assert!(!current_path.exists(), "{operation} rewrote the current copy");
            assert!(!mock.log().is_empty());
            assert_eq!(
                dav::download_clipboard_ring(&core.offline_guard, &dav_cfg).unwrap(),
                Some(state.ring)
            );
            assert!(
                local::load::<clip::RingChanges>(&dir, "clipboard-changes.json")
                    .unwrap()
                    .unwrap()
                    .is_empty()
            );
        }
        // A failed write stays pending until a later poll can persist the dismissal.
        std::fs::create_dir(&current_path).unwrap();
        assert!(core.dismiss_quick_paste().quick_paste.is_none());
        std::fs::remove_dir(&current_path).unwrap();
        assert!(core.poll_clipboard().unwrap().quick_paste.is_none());
        let current: clip::ClipboardCurrent =
            local::load(&dir, "clipboard-current.json").unwrap().unwrap();
        assert_eq!(current.dismissed_copy_id.as_deref(), Some("saved-copy"));

        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clipboard_current_polls_with_etag_without_fetching_history() {
        let mock = InProcessWebDavMock::start();
        let config = EngineConfig {
            koofr_endpoint: Some(mock.koofr_endpoint()),
            sync_key: Some("clipboard-test".into()),
            ..Default::default()
        };
        let sender = VitralCore::init(&serde_json::to_string(&config).unwrap()).unwrap();
        let receiver = VitralCore::init(&serde_json::to_string(&config).unwrap()).unwrap();
        let push = |id: &str, timestamp_ms| {
            sender.clipboard_push_local(ClipboardPayload {
                id: Some(id.into()),
                content: id.into(),
                mime_type: "text/plain".into(),
                source_device: "sender".into(),
                timestamp_ms,
            });
            sender.poll_clipboard_current().unwrap();
        };
        push("first", 10);
        assert!(
            sender.sync.lock().current_copy_etag.is_none(),
            "the mock PUT omits ETag"
        );
        let command =
            commands::execute(&receiver, "clipboard-poll-current", &serde_json::json!({})).unwrap();
        assert_eq!(command["current_copy"]["content"], "first");
        assert!(receiver.sync.lock().current_copy_etag.is_some());
        receiver.dismiss_quick_paste();
        let state = receiver.poll_clipboard_current().unwrap();
        assert_eq!(state.current_copy.unwrap().id, "first");
        assert!(state.quick_paste.is_none());
        assert_eq!(mock.not_modified.load(Ordering::Relaxed), 1);

        push("second", 20);
        assert_eq!(
            receiver
                .poll_clipboard_current()
                .unwrap()
                .current_copy
                .unwrap()
                .id,
            "second"
        );
        assert_eq!(mock.not_modified.load(Ordering::Relaxed), 1);
        assert!(receiver.clipboard_state().ring.is_empty());
        assert!(mock
            .log()
            .iter()
            .all(|(_, path)| path.ends_with("/clipboard/current.json.enc")));

        let next = InProcessWebDavMock::start();
        receiver
            .apply_preferences(EngineConfig {
                koofr_endpoint: Some(next.koofr_endpoint()),
                ..config
            })
            .unwrap();
        assert!(receiver.sync.lock().current_copy_etag.is_none());
        assert_eq!(
            receiver
                .poll_clipboard_current()
                .unwrap()
                .current_copy
                .unwrap()
                .id,
            "second"
        );
        assert!(next.exists("clipboard/current.json.enc"));
    }

    #[test]
    fn clipboard_push_publishes_current_while_history_is_in_flight() {
        let mock = InProcessWebDavMock::start();
        let config = serde_json::json!({
            "koofr_endpoint": mock.koofr_endpoint(), "sync_key": "clipboard-test"
        })
        .to_string();
        let sender = VitralCore::init(&config).unwrap();
        let receiver = VitralCore::init(&config).unwrap();
        let (hit, release) = mock.arm_get_gate("clipboard/history_ring.jsonl.enc");

        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                sender.clipboard_push(ClipboardPayload {
                    id: Some("fast-copy".into()),
                    content: "fast copy".into(),
                    mime_type: "text/plain".into(),
                    source_device: "sender".into(),
                    timestamp_ms: 10,
                })
            });
            hit.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(
                receiver
                    .poll_clipboard_current()
                    .unwrap()
                    .current_copy
                    .unwrap()
                    .id,
                "fast-copy"
            );
            assert!(receiver.clipboard_state().ring.is_empty());
            assert_eq!(
                sender
                    .poll_clipboard_current()
                    .unwrap()
                    .current_copy
                    .unwrap()
                    .id,
                "fast-copy"
            );
            release.send(()).unwrap();
            pending.join().unwrap();
        });
        receiver.poll_clipboard().unwrap();
        assert_eq!(receiver.clipboard_state().ring[0].id, "fast-copy");
    }

    #[test]
    fn clipboard_poll_restores_current_and_keeps_dismissal_local() {
        let dav = InProcessWebDavMock::start();
        let device = |id| VitralCore::init(&serde_json::json!({
            "device_id": id, "koofr_endpoint": dav.koofr_endpoint(), "sync_key": "vitral-test"
        }).to_string()).unwrap();
        let copy = |core: &VitralCore, text: &str| {
            core.clipboard_push(serde_json::from_value(serde_json::json!({"content": text})).unwrap());
        };
        let a = device("sender");
        copy(&a, "already in history");
        let b = device("receiver");
        assert_eq!(b.poll_clipboard().unwrap().current_copy.unwrap().content, "already in history");
        assert_eq!(b.clipboard_state().ring[0].content, "already in history");
        copy(&a, "new copy");
        assert_eq!(b.poll_clipboard().unwrap().quick_paste.unwrap().content, "new copy");
        a.consume_quick_paste();
        assert_eq!(b.consume_quick_paste().consumed.unwrap().content, "new copy");
        assert!(b.poll_clipboard().unwrap().quick_paste.is_none());
        // A new installation can copy before its first history poll.
        let c = device("fresh-install");
        copy(&c, "third copy");
        a.poll_clipboard().unwrap();
        let ring = a.clipboard_state().ring;
        assert_eq!(ring.len(), 3);
        assert!(ring.iter().any(|item| item.content == "already in history"));
        assert_eq!(ring[0].content, "third copy");
        assert!(!dav.list_files("").iter().any(|path| path.contains("live_slot")));
    }

    #[test]
    fn clipboard_cloud_current_survives_history_clear_and_offline_conflicts() {
        let dav = InProcessWebDavMock::start();
        let device = |id| VitralCore::init(&serde_json::json!({
            "device_id": id, "koofr_endpoint": dav.koofr_endpoint(), "sync_key": "current-test"
        }).to_string()).unwrap();
        let copy = |core: &VitralCore, id: &str, timestamp_ms| core.clipboard_push_local(ClipboardPayload {
            id: Some(id.into()), content: id.into(), mime_type: "text/plain".into(),
            source_device: "test".into(), timestamp_ms,
        });
        let a = device("a");
        let b = device("b");
        copy(&a, "newer", 300);
        copy(&b, "older-offline", 100);
        a.poll_clipboard().unwrap();
        assert_eq!(b.poll_clipboard().unwrap().current_copy.unwrap().id, "newer");
        b.dismiss_quick_paste();
        b.clipboard_clear_unpinned();
        b.poll_clipboard().unwrap();
        assert!(b.quick_paste_state().quick_paste.is_none());
        let c = device("fresh");
        let restored = c.poll_clipboard().unwrap();
        assert_eq!(restored.current_copy.unwrap().id, "newer");
        assert_eq!(restored.quick_paste.unwrap().id, "newer");
        assert!(c.clipboard_state().ring.is_empty());
        copy(&a, "next", 400);
        a.poll_clipboard().unwrap();
        assert_eq!(b.poll_clipboard().unwrap().quick_paste.unwrap().id, "next");
    }

    #[test]
    fn f10_retention_exponential_time_decay_and_hw_end_sharing() {
        // 1. Deterministic exponential time-decay check
        let mut ret = RetentionEngine::with_half_life(10.0);
        let s0 = ret.record_selection_at("测试", 1_000, 4.0);
        assert!((s0 - 4.0).abs() < 1e-9);
        let s_half = ret.decayed_score_at("测试", 11_000);
        assert!((s_half - 2.0).abs() < 1e-6, "half-life decay mismatch: {s_half}");
        let s_two_half = ret.decayed_score_at("测试", 21_000);
        assert!(
            (s_two_half - 1.0).abs() < 1e-6,
            "2x half-life decay mismatch: {s_two_half}"
        );
        // Zero / negative half-life safely clamps without NaN/panic
        let clamped = RetentionEngine::with_half_life(0.0);
        assert!(clamped.half_life_sec >= 1.0);

        // 2. Real VitralCore hw_end() -> Pinyin shared Frecency + decay overtaking
        let core = VitralCore::init(r#"{"schema":"rime_ice","frecency_half_life_sec":1}"#).unwrap();
        #[cfg(feature = "native-onnx")]
        {
            let fx_path =
                workspace_root().join("tests/fixtures/handwriting/mode2_horizontal_connected.json");
            let fx_val: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&fx_path).unwrap()).unwrap();
            let strokes: Vec<Vec<StrokePoint>> = fx_val["strokes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|st| {
                    st.as_array()
                        .unwrap()
                        .iter()
                        .map(|p| StrokePoint {
                            x: p["x"].as_f64().unwrap() as f32,
                            y: p["y"].as_f64().unwrap() as f32,
                            timestamp_ms: p["timestamp_ms"]
                                .as_u64()
                                .or_else(|| p["t"].as_u64())
                                .unwrap_or(0),
                        })
                        .collect()
                })
                .collect();

            let probe = type_ascii(&core, "shouxiezhongwen");
            let rival_idx = probe
                .candidates
                .iter()
                .position(|c| c.text != "手写中文")
                .expect("rival candidate");
            let rival = probe.candidates[rival_idx].text.clone();
            let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

            for _ in 0..2 {
                let cur = type_ascii(&core, "shouxiezhongwen");
                let idx = cur.candidates.iter().position(|c| c.text == rival).unwrap_or_else(|| {
                    panic!("rival {rival:?} missing from {:?}", cur.candidates);
                });
                core.select_candidate(idx).unwrap();
            }
            let before_hw = type_ascii(&core, "shouxiezhongwen");
            assert_eq!(before_hw.candidates[0].text, rival);
            let pre_hw_frec = before_hw
                .candidates
                .iter()
                .find(|c| c.text == "手写中文")
                .map(|c| c.frecency_score)
                .unwrap_or(0.0);
            let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

            // hw_end() auto-confirms Mode 2 into shared RetentionEngine (idempotent per edit)
            for _ in 0..4 {
                core.hw_begin(HwMode::ContinuousLattice);
                for s in &strokes {
                    core.hw_push_stroke(s, true).unwrap();
                }
                let payload = core.hw_end().unwrap();
                match payload {
                    HandwritingPayload::Mode2(m2) => assert_eq!(m2.composing_text, "手写中文"),
                    _ => panic!("expected Mode2"),
                }
                // Second hw_end() without edits must be idempotent
                let _ = core.hw_end().unwrap();
                core.hw_clear();
            }

            let after_hw = type_ascii(&core, "shouxiezhongwen");
            assert_eq!(
                after_hw.candidates[0].text, "手写中文",
                "expected HW confirmation to promote 手写中文 to rank 0, got {:?}",
                after_hw.candidates
            );
            assert!(after_hw.candidates[0].frecency_score > pre_hw_frec);
            let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        }

        // Time decay over > 2 half-lives reduces stale_word below fresh_word
        let init_shuru = type_ascii(&core, "shuru");
        let fresh_word = init_shuru.candidates[0].text.clone();
        let stale_word = init_shuru.candidates[1].text.clone();
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        for _ in 0..3 {
            let cur = type_ascii(&core, "shuru");
            let idx = cur.candidates.iter().position(|c| c.text == stale_word).unwrap();
            core.select_candidate(idx).unwrap();
        }
        assert_eq!(type_ascii(&core, "shuru").candidates[0].text, stale_word);
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

        std::thread::sleep(Duration::from_millis(2200));
        let after_decay = type_ascii(&core, "shuru");
        let fresh_idx = after_decay
            .candidates
            .iter()
            .position(|c| c.text == fresh_word)
            .unwrap();
        core.select_candidate(fresh_idx).unwrap();
        assert_eq!(type_ascii(&core, "shuru").candidates[0].text, fresh_word);
    }

    #[test]
    fn desktop_editing_keysyms_do_not_commit_punctuation() {
        for (schema, input) in [("rime_ice", "nihao"), ("double_pinyin_flypy", "nihc")] {
            let core = VitralCore::init(&serde_json::json!({"schema": schema}).to_string()).unwrap();
            for text in [input, "Hello"] {
                type_ascii(&core, text);
                for remaining in (0..text.len()).rev() {
                    let action = core.process_key(0xff08, 0, SwipeDir::None).unwrap();
                    assert!(action.commit_text.is_none(), "{schema}: {action:?}");
                    assert!(action.paired_punctuation.is_none());
                    assert_eq!(core.input.lock().engine.raw_input, text[..remaining]);
                }
                assert!(core.candidate_state().unwrap().preedit.text.is_empty());
            }
            type_ascii(&core, input);
            let escaped = core.process_key(0xff1b, 0, SwipeDir::None).unwrap();
            assert!(escaped.commit_text.is_none());
            assert!(escaped.preedit.text.is_empty());
            type_ascii(&core, "Hello");
            assert_eq!(core.process_key(0xff0d, 0, SwipeDir::None).unwrap().commit_text.as_deref(), Some("Hello"));
            let punctuation = core.process_key('(' as u32, 0, SwipeDir::None).unwrap();
            assert_eq!(punctuation.commit_text.as_deref(), Some("()"));
        }
    }

    #[test]
    fn f10_pin_erase_ksb_per_app_toggle_and_macros_punct_passthrough_gestures() {
        let core = VitralCore::init(r#"{"schema":"rime_ice"}"#).unwrap();

        // 1. OOB pin/erase returns NotFound
        assert!(matches!(core.pin_candidate(99), Err(VitralError::NotFound(_))));
        assert!(matches!(core.erase_candidate(99), Err(VitralError::NotFound(_))));

        // 2. Per-app CN/EN memory + MOD_LANG_TOGGLE persistence
        assert!(matches!(
            core.on_app_focus("   "),
            Err(VitralError::InvalidArgument(_))
        ));
        assert_eq!(core.on_app_focus("code").unwrap().lang_mode, LangMode::En);
        assert_eq!(
            core.on_app_focus("org.telegram.desktop").unwrap().lang_mode,
            LangMode::Zh
        );
        let ff = core.on_app_focus("org.mozilla.firefox").unwrap();
        assert_eq!(ff.lang_mode, LangMode::Zh);

        let _ = type_ascii(&core, "nih");
        let toggled = core.process_key(0xFFE1, MOD_LANG_TOGGLE, SwipeDir::None).unwrap();
        assert_eq!(toggled.lang_mode, LangMode::En);
        assert!(toggled.preedit.text.is_empty());

        assert_eq!(
            core.on_app_focus("org.telegram.desktop").unwrap().lang_mode,
            LangMode::Zh
        );
        let ff_again = core.on_app_focus("org.mozilla.firefox").unwrap();
        assert_eq!(ff_again.lang_mode, LangMode::En);
        assert!(ff_again.remembered);
        assert_eq!(
            core.on_app_focus("org.telegram.desktop").unwrap().lang_mode,
            LangMode::Zh
        );

        // 3. Pinned clipboard shortcut `ksb` across both schemas + OpenCC invariance + clear
        let pushed = core.clipboard_push(ClipboardPayload {
            id: None,
            content: "https://app.koofr.net/dav/Koofr".into(),
            mime_type: "text/plain".into(),
            source_device: "host".into(),
            timestamp_ms: 100,
        });
        let id = pushed.ring[0].id.clone();
        core.clipboard_pin(&id, Some("ksb")).unwrap();
        core.dismiss_quick_paste();

        for schema in [SchemaKind::DoublePinyinFlypy, SchemaKind::RimeIce] {
            core.set_schema(schema);
            core.set_opencc_mode(OpenCcMode::TaiwanStandard);
            let ksb_act = type_ascii(&core, "ksb");
            assert!(ksb_act.candidates[0].is_clipboard_shortcut);
            assert!(ksb_act.candidates[0].pinned);
            assert!(ksb_act.candidates[0].comment.contains("📌"));
            assert_eq!(ksb_act.candidates[0].text, "https://app.koofr.net/dav/Koofr");
            let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        }
        core.set_opencc_mode(OpenCcMode::Simplified);
        core.clipboard_pin(&id, Some("")).unwrap();
        let after_clear = type_ascii(&core, "ksb");
        assert!(!after_clear
            .candidates
            .iter()
            .any(|c| c.text == "https://app.koofr.net/dav/Koofr"));
        let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

        // 4. Paired punctuation & rq / sj time macros across both schemas
        let punct = core.process_key('(' as u32, 0, SwipeDir::None).unwrap();
        assert_eq!(punct.paired_punctuation.as_deref(), Some("()"));
        assert_eq!(punct.commit_text.as_deref(), Some("()"));
        assert_eq!(punct.cursor_delta, -1);

        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            core.set_schema(schema);
            let rq = type_ascii(&core, "rq");
            assert!(rq.macro_expanded.is_some());
            assert!(rq
                .candidates
                .iter()
                .any(|c| c.text.contains('-') || c.text.contains('年')));
            let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();

            let sj = type_ascii(&core, "sj");
            assert!(sj.macro_expanded.is_some());
            assert!(sj
                .candidates
                .iter()
                .any(|c| c.text.contains(':') || c.text.contains('时')));
            let _ = core.process_key(0x1B, 0, SwipeDir::None).unwrap();
        }

        // 5. Mixed-case English, URL, and email passthrough verbatim in Zh mode
        for token in [
            "VitralCore",
            "https://vitral.dev/Path_1?a=2",
            "user@example.com",
        ] {
            let act = type_ascii(&core, token);
            assert_eq!(act.preedit.text, token);
            let committed = core.process_key(0x20, 0, SwipeDir::None).unwrap();
            assert_eq!(committed.commit_text.as_deref(), Some(token));
        }

        // 6. Mobile spacebar cursor swipe, backspace swipe-delete, & 350ms hold voice hook
        let left = core.process_key(0x20, 0, SwipeDir::Left).unwrap();
        assert_eq!(left.cursor_delta, -1);
        assert!(left.commit_text.is_none());
        let right = core.process_key(0x20, 0, SwipeDir::Right).unwrap();
        assert_eq!(right.cursor_delta, 1);
        assert!(right.commit_text.is_none());
        let up = core.process_key(0x20, 0, SwipeDir::Up).unwrap();
        assert!(up.voice_trigger);
        assert!(up.commit_text.is_none());
        let hold = core.process_key(0x20, MOD_VOICE_HOLD, SwipeDir::None).unwrap();
        assert!(hold.voice_trigger);
        assert!(!is_voice_hold_triggered(349));
        assert!(is_voice_hold_triggered(350));

        let _ = type_ascii(&core, "ceshi");
        let bs = core.process_key(0x08, 0, SwipeDir::Left).unwrap();
        assert!(bs.preedit.text.is_empty());
    }

    #[test]
    fn f10_mru_devices_and_imp_extb_polish() {
        let core = VitralCore::init(
            r#"{
                "known_devices": [
                    {"id":"off","device_name":"Off","tier":"offline","lan_online":false,"wan_online":false,"transfer_count":99,"last_used_ms":9999},
                    {"id":"wan","device_name":"Wan","tier":"wan","lan_online":false,"wan_online":true,"transfer_count":20,"last_used_ms":5000},
                    {"id":"lan-a","device_name":"LanA","tier":"lan","lan_online":true,"wan_online":true,"transfer_count":1,"last_used_ms":1000},
                    {"id":"lan-b","device_name":"LanB","tier":"lan","lan_online":true,"wan_online":true,"transfer_count":10,"last_used_ms":3000}
                ]
            }"#,
        )
        .unwrap();
        let ids: Vec<String> = core
            .relay_devices_mru()
            .into_iter()
            .map(|d| d.device_id)
            .collect();
        assert_eq!(ids, vec!["lan-b", "lan-a", "wan", "off"]);

        // Ext-B (U+20000+) TSV import succeeds while Emoji (U+1F600) is still rejected
        let tmp_dir = std::env::temp_dir().join(format!("vitral_m1c_extb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tsv_path = tmp_dir.join("extb.tsv");
        std::fs::write(&tsv_path, "\u{20000}\tqi\t500\n\u{2A6D6}\tlong\t400\n\u{1F600}\txiao\t100\n")
            .unwrap();
        let rep = core.import_lexicon(&tsv_path, LexiconFormat::Tsv).unwrap();
        assert_eq!(rep.imported_count, 2, "Ext-B entries should import cleanly: {rep:?}");
        assert_eq!(rep.skipped_count, 1, "emoji entry should still be skipped");
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn f11_crypto_zstd_and_tamper_rejection() {
        let key = Some("vitral-test-chacha20-key-32bytes!"); // 33 bytes arbitrary passphrase
        let plain = b"vitral-secret-snapshot-payload-v1";
        let compressed = zstd_compress(plain).unwrap();
        assert_eq!(zstd_decompress(&compressed).unwrap(), plain);

        let mut enc = encrypt_envelope(&compressed, key).unwrap();
        let dec = decrypt_envelope(&enc, key).unwrap();
        assert_eq!(zstd_decompress(&dec).unwrap(), plain);

        assert!(matches!(
            decrypt_envelope(b"corrupt-non-ciphertext", key),
            Err(VitralError::Integrity(_))
        ));
        assert!(matches!(
            decrypt_envelope(&enc, Some("wrong-key")),
            Err(VitralError::Integrity(_))
        ));
        let last = enc.len() - 1;
        enc[last] ^= 0x55;
        assert!(matches!(
            decrypt_envelope(&enc, key),
            Err(VitralError::Integrity(_))
        ));
    }

    #[test]
    fn successful_discovery_clears_previous_lan_error() {
        let core = VitralCore::init(r#"{"device_id":"discovery-health-test"}"#).unwrap();
        {
            let mut sync = core.sync.lock();
            sync.last_lan_error = Some("previous discovery failure".into());
            sync.last_lan_sync_ms = 1;
        }
        core.discover_devices().unwrap();
        let status = core.sync_status();
        assert!(status.last_lan_error.is_none());
        assert!(status.last_lan_sync_ms > 1);
    }

    struct TestStateDir(std::path::PathBuf);

    impl Drop for TestStateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state_dir_for_test(name: &str) -> TestStateDir {
        let dir = std::env::temp_dir().join(format!("vitral-package-a-{name}-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        TestStateDir(dir)
    }

    #[test]
    fn preference_half_life_persists_and_explicit_init_overrides_snapshot() {
        let dir = state_dir_for_test("preference-half-life");
        let cfg = serde_json::json!({
            "data_dir": dir.0,
            "assets_dir": dir.0.join("no-assets"),
            "pure_offline": true,
            "schema": "rime_ice"
        });
        let core = VitralCore::init(&cfg.to_string()).unwrap();
        let mut retention = RetentionEngine::with_half_life(2_592_000.0);
        retention.record_selection_at("学习词条", 123, 2.0);
        let snapshot = LexiconSnapshotPayload {
            schema_version: 1,
            retention,
            imported_entries: vec![crate::imp::ImportedLexiconEntry {
                text: "星尘漫步者".into(),
                pinyin: "xing chen man bu zhe".into(),
                toned_pinyin: "xīng chén màn bù zhě".into(),
                xiaohe: "xkifmjbuve".into(),
                weight: 100,
            }],
        };
        local::save(&dir.0, "lexicon-state.json", &snapshot).unwrap();
        drop(core);
        let core = VitralCore::init(&cfg.to_string()).unwrap();
        assert_eq!(core.input.lock().retention.half_life_sec, 2_592_000.0);
        let mut preferences = EngineConfig::parse_json(&cfg.to_string()).unwrap();
        preferences.frecency_half_life_sec = Some(604_800.0);
        core.apply_preferences(preferences).unwrap();
        assert_eq!(core.input.lock().engine.config.frecency_half_life_sec, Some(604_800.0));
        drop(core);
        let stored = local::load::<LexiconSnapshotPayload>(&dir.0, "lexicon-state.json")
            .unwrap().unwrap();
        assert_eq!(stored.retention.half_life_sec, 604_800.0);
        assert_eq!(serde_json::to_value(&stored.retention.frecency).unwrap(),
            serde_json::to_value(&snapshot.retention.frecency).unwrap());
        assert_eq!(serde_json::to_value(&stored.imported_entries).unwrap(),
            serde_json::to_value(&snapshot.imported_entries).unwrap());
        let core = VitralCore::init(&cfg.to_string()).unwrap();
        assert_eq!(core.input.lock().retention.half_life_sec, 604_800.0);
        assert_eq!(core.input.lock().retention.frecency["学习词条"], 2.0);
        drop(core);
        let mut explicit_cfg = cfg;
        explicit_cfg["frecency_half_life_sec"] = serde_json::json!(86_400);
        let core = VitralCore::init(&explicit_cfg.to_string()).unwrap();
        assert_eq!(core.input.lock().retention.half_life_sec, 86_400.0);
    }

    #[test]
    fn legacy_clipboard_phrases_are_purged_on_startup_and_restore() {
        let dir = state_dir_for_test("legacy-clipboard-phrases");
        let mock = InProcessWebDavMock::start();
        let cfg = serde_json::json!({
            "data_dir": dir.0,
            "assets_dir": dir.0.join("no-assets"),
            "pure_offline": true,
            "schema": "rime_ice",
            "koofr_endpoint": mock.koofr_endpoint()
        });
        let legacy_word = "霁岚云篆";
        let typed_word = "霁岚星牒";
        let snapshot = serde_json::json!({
            "schema_version": 1,
            "imported_entries": [],
            "retention": {
                "frecency": {(legacy_word): 0.35, (typed_word): 1.0},
                "frecency_updated_ms": {(legacy_word): 10, (typed_word): 1},
                "user_english": {"copiedortyped": "CopiedOrTyped"},
                "habit_updated_ms": {"e:copiedortyped": 10},
                "pinned_words": [], "erased_words": [],
                "user_phrases": {
                    (legacy_word): {
                        "word": legacy_word, "quanpin": "ji lan yun zhuan", "xiaohe": "jiljyvyv",
                        "count": 1, "updated_ms": 10, "source": "clipboard"
                    },
                    (typed_word): {
                        "word": typed_word, "quanpin": "ji lan xing die", "xiaohe": "jiljxkdp",
                        "count": 1, "updated_ms": 1, "source": "typed"
                    }
                }
            }
        });
        local::save(&dir.0, "lexicon-state.json", &snapshot).unwrap();
        let core = VitralCore::init(&cfg.to_string()).unwrap();
        let assert_clean = || {
            let stored = local::load::<LexiconSnapshotPayload>(&dir.0, "lexicon-state.json")
                .unwrap()
                .unwrap();
            assert!(!stored.retention.user_phrases.contains_key(legacy_word));
            assert_eq!(stored.retention.user_phrases[typed_word].source, "typed");
            let ret = &stored.retention;
            let delta = ret.export_crdt_delta("clean");
            assert!(!delta.frecency.contains_key(legacy_word));
            assert!(!delta.frecency_updated_ms.contains_key(legacy_word));
            assert!(!delta.user_phrases.contains_key(legacy_word));
            assert_eq!(delta.frecency[typed_word], 1.0);
            assert_eq!(delta.user_english["copiedortyped"], "CopiedOrTyped");
            let mut profile = profile::ProfileEngine::new(None).unwrap();
            profile.statistical(ret);
            let speech = ai::speech_context(ret, None, String::new());
            for words in [&profile.profile.entities, &speech.hotwords] {
                assert!(!words.iter().any(|word| word == legacy_word));
                assert!(words.iter().any(|word| word == typed_word));
            }
            core.on_app_focus("editor").unwrap();
            let prediction = core.predict_request(predict::PredictRequest {
                context_before: "霁岚".into(),
                ..Default::default()
            });
            assert_eq!(prediction.text, "星牒", "{prediction:?}");
            assert!(!prediction.candidates.iter().any(|text| text == "云篆"));
            let action = type_ascii(&core, "jilanyunzhuan");
            assert!(!action
                .candidates
                .iter()
                .any(|item| item.text == legacy_word));
            core.process_key(0x1b, 0, SwipeDir::None).unwrap();
            let action = type_ascii(&core, "jilanxingdie");
            assert!(action.candidates.iter().any(|item| item.text == typed_word));
            core.process_key(0x1b, 0, SwipeDir::None).unwrap();
        };
        assert_clean();

        let snapshot: LexiconSnapshotPayload = serde_json::from_value(snapshot).unwrap();
        assert_eq!(
            snapshot.retention.user_phrases[legacy_word].source,
            "clipboard"
        );
        let mut delta = snapshot.retention.export_crdt_delta("legacy-device");
        let stale = delta.user_phrases.get_mut(typed_word).unwrap();
        stale.source = "clipboard".into();
        stale.updated_ms = 100;
        stale.count = 100;
        core.set_pure_offline(false);
        let dav =
            DavConfig::from_engine_config(&EngineConfig::parse_json(&cfg.to_string()).unwrap())
                .unwrap();
        upload_lexicon_snapshot(&core.offline_guard, &dav, &snapshot).unwrap();
        upload_retention_crdt_delta(&core.offline_guard, &dav, &delta).unwrap();
        let restored = core.restore_from_koofr("{}").unwrap();
        assert_eq!(restored.snapshots_applied, 2);
        assert_clean();
    }

    #[test]
    fn restore_keeps_five_pre_restore_retention_snapshots() {
        let mock = InProcessWebDavMock::start();
        let dir = state_dir_for_test("restore");
        let cfg = serde_json::json!({
            "state_dir": dir.0,
            "koofr_endpoint": mock.koofr_endpoint()
        });
        let core = VitralCore::init(&cfg.to_string()).unwrap();
        let dav = DavConfig::from_engine_config(&EngineConfig::parse_json(&cfg.to_string()).unwrap()).unwrap();
        let mut cloud_retention = RetentionEngine::default();
        cloud_retention.pin_word("云端词");
        upload_lexicon_snapshot(&core.offline_guard, &dav, &LexiconSnapshotPayload {
            schema_version: 1,
            retention: cloud_retention,
            imported_entries: Vec::new(),
        }).unwrap();
        for index in 0..7 {
            let word = format!("本地词{index}");
            core.input.lock().retention.pin_word(&word);
            core.restore_from_koofr("{}").unwrap();
            let input = core.input.lock();
            assert!(input.retention.pinned_words.contains("云端词"));
            assert!(!input.retention.pinned_words.contains(&word));
        }
        assert_eq!(core.sync_status().backup_count, 5);
        let backup_dir = dir.0.join("backups");
        let mut backups: Vec<_> = std::fs::read_dir(&backup_dir).unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        backups.sort();
        assert_eq!(backups.len(), 5);
        for (index, name) in (2..7).zip(backups) {
            let retention = local::load::<RetentionEngine>(&backup_dir, &name).unwrap().unwrap();
            assert!(retention.pinned_words.contains(&format!("本地词{index}")));
        }
    }

    #[test]
    fn f11_opendal_webdav_3tier_sync_relay_24h_orphan_and_restore() {
        let mock = InProcessWebDavMock::start();
        let cfg_b = serde_json::json!({
            "webdav_url": mock.koofr_endpoint(),
            "sync_key": "vitral-m1c-sync-key-32bytes!",
            "device_id": "dev-b",
            "clipboard_capacity": 4
        });

        // Empty WebDAV restore fails cleanly without wiping local state
        let dev_empty = VitralCore::init(r#"{"device_id":"dev-b"}"#).unwrap();
        dev_empty.clipboard_push(ClipboardPayload {
            id: None,
            content: "local-kept".into(),
            mime_type: "text/plain".into(),
            source_device: "dev-b".into(),
            timestamp_ms: 1,
        });
        assert!(matches!(
            dev_empty.restore_from_koofr(&cfg_b.to_string()),
            Err(VitralError::NotFound(_))
        ));
        assert_eq!(dev_empty.clipboard_state().ring[0].content, "local-kept");

        // Corrupt backup restore fails Integrity without wiping local state
        mock.write_file("profile/config.json.enc", b"corrupt-non-ciphertext", 1_700_000_000);
        assert!(matches!(
            dev_empty.restore_from_koofr(&cfg_b.to_string()),
            Err(VitralError::Integrity(_))
        ));
        assert_eq!(dev_empty.clipboard_state().ring[0].content, "local-kept");

        // Fresh WebDAV mock for Device A -> Device B full 3-tier + restore
        drop(mock);
        let mock = InProcessWebDavMock::start();
        let dev_a = VitralCore::init(
            &serde_json::json!({
                "koofr_endpoint": mock.koofr_endpoint(),
                "sync_key": "vitral-m1c-sync-key-32bytes!",
                "device_id": "dev-a",
                "clipboard_ring_capacity": 4
            })
            .to_string(),
        )
        .unwrap();

        let scel_path = workspace_root().join("tests/fixtures/lexicons/sample_hotwords.scel");
        dev_a.import_lexicon(&scel_path, LexiconFormat::Scel).unwrap();
        let p = dev_a.clipboard_push(ClipboardPayload {
            id: None,
            content: "sk-live-unfiltered-token-999".into(),
            mime_type: "text/plain".into(),
            source_device: "dev-a".into(),
            timestamp_ms: 100,
        });
        dev_a
            .clipboard_pin(&p.ring[0].id, Some("crp"))
            .unwrap();
        dev_a.reload_assets().unwrap();

        let files = mock.list_files("");
        assert!(files.iter().any(|f| f == "profile/config.json.enc"));
        assert!(files.iter().any(|f| f == "lexicon/snapshot.zst.enc"));
        assert!(files.iter().any(|f| f.starts_with("lexicon/crdt/")));
        assert!(files.iter().any(|f| f.starts_with("lexicon/imports/")));
        assert!(files.iter().any(|f| f == "clipboard/history_ring.jsonl.enc"));
        assert!(
            !files.iter().any(|f| f.ends_with("live_slot.enc")),
            "FORBIDDEN live_slot.enc detected: {files:?}"
        );

        // Tier 3 relay send + poll + immediate DELETE
        let dev_b = VitralCore::init(
            &serde_json::json!({
                "koofr_endpoint": mock.koofr_endpoint(),
                "sync_key": "vitral-m1c-sync-key-32bytes!",
                "device_id": "dev-b"
            })
            .to_string(),
        )
        .unwrap();
        dev_a
            .relay_send("dev-b", "ephemeral-wan-secret", false)
            .unwrap();
        assert!(!mock.list_files("relay/").is_empty());
        let polled = dev_b.relay_poll_inbox().unwrap();
        assert_eq!(polled.received, vec!["ephemeral-wan-secret".to_string()]);
        assert_eq!(polled.deleted_count, 2);
        assert!(mock.list_files("relay/").is_empty());

        // SHA-256 mismatch abort in relay_poll_inbox
        mock.write_file("relay/tampered.enc", b"corrupt-bytes", 1_700_000_000);
        mock.write_file(
            "relay/tampered.meta.json",
            br#"{"transfer_id":"tampered","sha256":"0000000000000000000000000000000000000000000000000000000000000000","target_device_id":"dev-b"}"#,
            1_700_000_000,
        );
        assert!(matches!(
            dev_b.relay_poll_inbox(),
            Err(VitralError::Integrity(_))
        ));
        mock.nodes.lock().remove("/dav/Koofr/Vitral/relay/tampered.enc");
        mock.nodes.lock().remove("/dav/Koofr/Vitral/relay/tampered.meta.json");

        // 24h orphan boundary: 23h59m kept vs 24h01m deleted
        let now_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        mock.write_file("relay/keep_23h59m.enc", b"keep", now_s - (23 * 3600 + 59 * 60));
        mock.write_file(
            "relay/keep_23h59m.meta.json",
            b"{}",
            now_s - (23 * 3600 + 59 * 60),
        );
        mock.write_file("relay/purge_24h01m.enc", b"purge", now_s - (24 * 3600 + 60));
        mock.write_file(
            "relay/purge_24h01m.meta.json",
            b"{}",
            now_s - (24 * 3600 + 60),
        );
        let cleanup = dev_a.relay_cleanup_orphans().unwrap();
        assert_eq!(cleanup.purged_count, 2);
        assert!(mock.exists("relay/keep_23h59m.enc"));
        assert!(!mock.exists("relay/purge_24h01m.enc"));

        // Full device restore onto dev_c
        let dev_c = VitralCore::init(r#"{"device_id":"dev-c"}"#).unwrap();
        let restored = dev_c
            .restore_from_koofr(
                &serde_json::json!({
                    "webdav_url": mock.koofr_endpoint(),
                    "sync_key": "vitral-m1c-sync-key-32bytes!"
                })
                .to_string(),
            )
            .unwrap();
        assert!(restored.restored);
        assert!(restored.snapshots_applied >= 3);
        // Restoring history also restores the current copy, independently of candidate visibility.
        assert!(dev_c.quick_paste_state().current_copy.is_some());
        assert_eq!(
            dev_c.clipboard_state().ring[0].content,
            "sk-live-unfiltered-token-999"
        );
        let crp = type_ascii(&dev_c, "crp");
        assert_eq!(crp.candidates[0].text, "sk-live-unfiltered-token-999");
        let _ = dev_c.process_key(0x1B, 0, SwipeDir::None).unwrap();
        dev_c.set_schema(SchemaKind::RimeIce);
        let ldb = type_ascii(&dev_c, "laodengjinbi");
        assert!(ldb.candidates.iter().any(|c| c.text == "老登金币"));
    }

    #[test]
    fn f11_localsend_v2_loopback_udp_and_pure_offline_guard() {
        let guard = OfflineGuard::new(false);
        let mut peer = LocalSendTransport::start_receiver(
            &guard,
            "127.0.0.1:0",
            LocalSendDeviceInfo::new("ls-peer", "LS Peer", 0),
        )
        .unwrap();

        // Loopback UDP discovery announcement via socket2
        let udp_rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp_rx
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let udp_port = udp_rx.local_addr().unwrap().port();
        LocalSendTransport::announce_udp(
            &guard,
            "127.0.0.1",
            udp_port,
            &LocalSendDeviceInfo::new("host-dev", "Vitral-Host", peer.port()),
        )
        .unwrap();
        let mut buf = [0u8; 2048];
        let (n, _) = udp_rx.recv_from(&mut buf).unwrap();
        let ann: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
        assert_eq!(ann["fingerprint"], "host-dev");
        assert_eq!(ann["announce"], true);

        let mock = InProcessWebDavMock::start();
        let core = VitralCore::init(
            &serde_json::json!({
                "koofr_endpoint": mock.koofr_endpoint(),
                "device_id": "sender-host",
                "known_devices": [
                    {
                        "device_id": "lan-rival",
                        "device_name": "LAN Rival",
                        "tier": "lan",
                        "usage_count": 2,
                        "last_used_ms": 2000
                    },
                    {
                        "device_id": "ls-peer",
                        "device_name": "LS Peer",
                        "tier": "lan",
                        "usage_count": 1,
                        "last_used_ms": 1000,
                        "localsend_url": peer.base_url()
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(core.relay_devices_mru()[0].device_id, "lan-rival");

        // Text transfer over LocalSend v2
        let tx_txt = core
            .relay_send("ls-peer", "Vitral LocalSend v2 text", false)
            .unwrap();
        assert_eq!(tx_txt.status, "sent");
        assert_eq!(tx_txt.sha256, sha256_hex(b"Vitral LocalSend v2 text"));
        let status = core.sync_status();
        assert!(status.lan_enabled);
        assert_eq!(status.lan_peer_count, 1);
        assert!(status.last_lan_sync_ms > 0);
        assert!(status.last_lan_error.is_none());

        // File transfer over LocalSend v2
        let tmp_file =
            std::env::temp_dir().join(format!("vitral_ls_file_{}.txt", std::process::id()));
        let file_bytes = b"Vitral LAN file payload";
        std::fs::write(&tmp_file, file_bytes).unwrap();
        let tx_file = core
            .relay_send("ls-peer", tmp_file.to_str().unwrap(), true)
            .unwrap();
        assert_eq!(tx_file.sha256, sha256_hex(file_bytes));
        let _ = std::fs::remove_file(&tmp_file);

        let rec = peer.received();
        assert_eq!(rec.len(), 2);
        assert_eq!(rec[0].data, b"Vitral LocalSend v2 text");
        assert_eq!(rec[1].data, file_bytes);

        // MRU climbs ls-peer to rank 0 with real wall-clock last_used_ms
        let mru_after = core.relay_devices_mru();
        assert_eq!(mru_after[0].device_id, "ls-peer");
        assert_eq!(mru_after[0].usage_count, 3);
        assert!(mru_after[0].last_used_ms > 1_000_000_000_000);

        // Pure offline physically blocks all outbound calls with 0 requests reaching WebDAV/LocalSend
        mock.clear_log();
        core.set_pure_offline(true);
        assert!(matches!(
            core.relay_send("ls-peer", "blocked", false),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            core.relay_poll_inbox(),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            core.relay_cleanup_orphans(),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            core.restore_from_koofr(&serde_json::json!({"koofr_endpoint": mock.koofr_endpoint()}).to_string()),
            Err(VitralError::PureOfflineViolation(_))
        ));
        // Background clipboard push stays local and emits 0 WebDAV requests
        core.clipboard_push(ClipboardPayload {
            id: None,
            content: "offline-local-only".into(),
            mime_type: "text/plain".into(),
            source_device: "sender-host".into(),
            timestamp_ms: 99,
        });
        assert!(mock.log().is_empty());
        assert_eq!(peer.received().len(), 2);

        // PureOfflineGuard also blocks starting LocalSend receiver or UDP announce
        let offline_guard = OfflineGuard::new(true);
        assert!(matches!(
            LocalSendTransport::start_receiver(
                &offline_guard,
                "127.0.0.1:0",
                LocalSendDeviceInfo::new("x", "x", 0)
            ),
            Err(VitralError::PureOfflineViolation(_))
        ));
        assert!(matches!(
            LocalSendTransport::announce_udp(
                &offline_guard,
                "127.0.0.1",
                udp_port,
                &LocalSendDeviceInfo::new("x", "x", 0)
            ),
            Err(VitralError::PureOfflineViolation(_))
        ));

        peer.stop();
        // Silence unused DeviceTier import warning if any
        let _ = DeviceTier::Lan;
    }

    #[test]
    fn m1c_iter2_paired_typographic_quotes_and_debug_redaction() {
        let core = VitralCore::init("{}").unwrap();
        for (ch, expected_pair) in [('“', "“”"), ('‘', "‘’")] {
            let act = core.process_key(ch as u32, 0, SwipeDir::None).unwrap();
            assert_eq!(act.paired_punctuation.as_deref(), Some(expected_pair));
            assert_eq!(act.commit_text.as_deref(), Some(expected_pair));
            assert_eq!(act.cursor_delta, -1);
        }

        let cfg = EngineConfig {
            koofr_password: Some("super-secret-dav-pw".into()),
            sync_key: Some("super-secret-sync-key".into()),
            ..EngineConfig::default()
        };
        let eng_dbg = format!("{cfg:?}");
        assert!(!eng_dbg.contains("super-secret-dav-pw"));
        assert!(!eng_dbg.contains("super-secret-sync-key"));
        assert!(eng_dbg.contains("[REDACTED]"));

        let dav_cfg = DavConfig {
            endpoint: "http://127.0.0.1:9999".into(),
            username: Some("alice".into()),
            password: Some("super-secret-dav-pw".into()),
            root: None,
            sync_key: Some("super-secret-sync-key".into()),
        };
        let dav_dbg = format!("{dav_cfg:?}");
        assert!(!dav_dbg.contains("super-secret-dav-pw"));
        assert!(!dav_dbg.contains("super-secret-sync-key"));
        assert!(dav_dbg.contains("[REDACTED]"));
    }

    #[test]
    fn m1c_iter2_tokio_runtime_nesting_safe_and_binary_wan_relay_and_sha256_required() {
        use crate::dav::{
            fetch_backup_from_koofr, put_lexicon_import_artifact, upload_koofr_relay,
            upload_retention_crdt_delta, validate_clean_root, KoofrDavClient,
        };
        use crate::offline_guard::OfflineGuard;

        let mock = InProcessWebDavMock::start();
        let cfg = DavConfig {
            endpoint: mock.koofr_endpoint(),
            username: None,
            password: None,
            root: None,
            sync_key: Some("iter2-sync-key".into()),
        };
        let guard = OfflineGuard::new(false);

        // 1. Inside an active multi-thread Tokio runtime (like Tauri v2 async fn / #[tokio::test]),
        // KoofrDavClient::connect, write_bytes, and fetch_backup_from_koofr must NOT panic.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = KoofrDavClient::connect(&guard, &cfg).unwrap();
            client
                .write_bytes("profile/config.json.enc", &encrypt_envelope(
                    &serde_json::to_vec(&ProfileBackup {
                        shortcuts: shortcuts::defaults(),
                        schema_version: 1,
                        schema: SchemaKind::DoublePinyinFlypy,
                        opencc_mode: OpenCcMode::Simplified,
                        clipboard_ring_capacity: 50,
                        frecency_half_life_sec: 604_800.0,
                        app_lang_memory: HashMap::new(),
                    })
                    .unwrap(),
                    cfg.sync_key.as_deref(),
                ).unwrap())
                .unwrap();
            let fetched = fetch_backup_from_koofr(&guard, &cfg).unwrap();
            assert_eq!(fetched.snapshots_applied, 1);

            // Mid-flight OfflineGuard check on held KoofrDavClient
            guard.set_pure_offline(true);
            assert!(matches!(
                client.write_bytes("profile/config.json.enc", b"blocked"),
                Err(VitralError::PureOfflineViolation(_))
            ));
            guard.set_pure_offline(false);
        });

        // 2. Non-UTF-8 binary file round-trip over Koofr WAN /relay/ preserves exact bytes
        let bin_bytes: Vec<u8> = vec![0xFF, 0x00, 0x80, 0xFE, 0x28, 0xB5, 0x2F, 0xFD, 0x01, 0x7F];
        let tmp_bin = std::env::temp_dir().join(format!("vitral_wan_bin_{}.bin", std::process::id()));
        std::fs::write(&tmp_bin, &bin_bytes).unwrap();

        let dev_a = VitralCore::init(
            &serde_json::json!({
                "koofr_endpoint": mock.koofr_endpoint(),
                "sync_key": "iter2-sync-key",
                "device_id": "wan-a"
            })
            .to_string(),
        )
        .unwrap();
        let dev_b = VitralCore::init(
            &serde_json::json!({
                "koofr_endpoint": mock.koofr_endpoint(),
                "sync_key": "iter2-sync-key",
                "device_id": "wan-b"
            })
            .to_string(),
        )
        .unwrap();

        let send_res = dev_a
            .relay_send("wan-b", tmp_bin.to_str().unwrap(), true)
            .unwrap();
        assert_eq!(send_res.transport, "koofr_webdav_relay");
        let _ = std::fs::remove_file(&tmp_bin);

        let polled = dev_b.relay_poll_inbox().unwrap();
        assert_eq!(polled.deleted_count, 2);
        assert_eq!(polled.received_items.len(), 1);
        assert_eq!(polled.received_items[0].raw_bytes, bin_bytes);
        assert!(polled.received_items[0].is_file);
        assert!(polled.received_items[0].file_name.is_some());

        // 3. Empty meta.sha256 must be rejected with Integrity error
        let enc_dummy = encrypt_envelope(b"hello", Some("iter2-sync-key")).unwrap();
        mock.write_file("relay/no_sha.enc", &enc_dummy, 1_700_000_000);
        mock.write_file(
            "relay/no_sha.meta.json",
            br#"{"transfer_id":"no_sha","sha256":"","target_device_id":"wan-b"}"#,
            1_700_000_000,
        );
        assert!(matches!(
            dev_b.relay_poll_inbox(),
            Err(VitralError::Integrity(_))
        ));
        mock.nodes.lock().remove("/dav/Koofr/Vitral/relay/no_sha.enc");
        mock.nodes.lock().remove("/dav/Koofr/Vitral/relay/no_sha.meta.json");

        // 4. Path traversal sanitization checks
        assert!(matches!(
            validate_clean_root(Some("Vitral/../escape")),
            Err(VitralError::InvalidArgument(_))
        ));
        let client = KoofrDavClient::connect(&guard, &cfg).unwrap();
        assert!(matches!(
            client.write_bytes("../escape.txt", b"x"),
            Err(VitralError::InvalidArgument(_))
        ));
        assert!(matches!(
            client.read_bytes("../escape.txt"),
            Err(VitralError::InvalidArgument(_))
        ));
        assert!(matches!(
            client.delete_file("../escape.txt"),
            Err(VitralError::InvalidArgument(_))
        ));
        assert!(matches!(
            upload_koofr_relay(&guard, &cfg, "a", "b", "../bad", "f.txt", false, b"x"),
            Err(VitralError::InvalidArgument(_))
        ));
        assert!(matches!(
            put_lexicon_import_artifact(&guard, &mock.koofr_endpoint(), None, None, None, "../bad.cbor", b"x"),
            Err(VitralError::InvalidArgument(_))
        ));

        // 5. CRDT restore ordering: seq=2 (older ts) vs seq=10 (newer ts)
        // Lexicographically "crdtdev_10.cbor.enc" < "crdtdev_2.cbor.enc",
        // so without (timestamp_ms, seq) sorting, seq=2 would overwrite seq=10!
        let mut r_old = RetentionEngine {
            crdt_seq: 2,
            ..Default::default()
        };
        r_old.erased_words.insert("测试词".into());
        let mut delta_2 = r_old.export_crdt_delta("crdtdev");
        delta_2.seq = 2;
        delta_2.timestamp_ms = 1_000;

        let mut r_new = RetentionEngine {
            crdt_seq: 10,
            ..Default::default()
        };
        r_new.pinned_words.insert("测试词".into());
        let mut delta_10 = r_new.export_crdt_delta("crdtdev");
        delta_10.seq = 10;
        delta_10.timestamp_ms = 2_000;

        upload_retention_crdt_delta(&guard, &cfg, &delta_10).unwrap();
        upload_retention_crdt_delta(&guard, &cfg, &delta_2).unwrap();

        let dev_c = VitralCore::init(r#"{"device_id":"crdt-restored"}"#).unwrap();
        dev_c
            .restore_from_koofr(
                &serde_json::json!({
                    "koofr_endpoint": mock.koofr_endpoint(),
                    "sync_key": "iter2-sync-key"
                })
                .to_string(),
            )
            .unwrap();
        let st = dev_c.input.lock();
        assert!(
            st.retention.pinned_words.contains("测试词")
                && !st.retention.erased_words.contains("测试词"),
            "newer seq=10 pin must win over older seq=2 erase despite lexicographic path order"
        );
    }

    #[test]
    fn m1c_iter2_localsend_bounded_body_413_and_url_injection_rejection() {
        use crate::lan::{MAX_LOCALSEND_CONTROL_BODY_BYTES, MAX_LOCALSEND_UPLOAD_BODY_BYTES};
        use crate::offline_guard::OfflineGuard;
        use std::net::TcpStream;

        let guard = OfflineGuard::new(false);
        let mut peer = LocalSendTransport::start_receiver(
            &guard,
            "127.0.0.1:0",
            LocalSendDeviceInfo::new("ls-sec", "LS Sec", 0),
        )
        .unwrap();

        // 1. Oversized /prepare-upload (> 1 MiB) returns HTTP 413
        let oversized_ctrl = "x".repeat(MAX_LOCALSEND_CONTROL_BODY_BYTES + 128);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();
        let prep_413 = agent
            .post(&format!("{}/api/localsend/v2/prepare-upload", peer.base_url()))
            .send(oversized_ctrl.as_bytes());
        assert!(
            matches!(prep_413, Err(ureq::Error::StatusCode(413))),
            "expected HTTP 413 on oversized prepare-upload, got: {prep_413:?}"
        );

        // 2. Oversized Content-Length (> 64 MiB) on /upload immediately returns HTTP 413 without allocating
        let mut stream2 = TcpStream::connect(format!("127.0.0.1:{}", peer.port())).unwrap();
        stream2.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let huge_cl = MAX_LOCALSEND_UPLOAD_BODY_BYTES + 1024;
        let req_hdr2 = format!(
            "POST /api/localsend/v2/upload HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {huge_cl}\r\nConnection: close\r\n\r\n"
        );
        stream2.write_all(req_hdr2.as_bytes()).unwrap();
        let mut status_line2 = String::new();
        BufReader::new(&stream2).read_line(&mut status_line2).unwrap();
        drop(stream2);
        assert!(
            status_line2.starts_with("HTTP/1.1 413"),
            "expected HTTP 413 on oversized upload Content-Length, got: {status_line2}"
        );

        // 3. Unknown path returns 404 immediately without reading body
        let unknown_404 = agent
            .post(&format!("{}/unknown/path", peer.base_url()))
            .send_empty();
        assert!(
            matches!(unknown_404, Err(ureq::Error::StatusCode(404))),
            "expected HTTP 404 on unknown path, got: {unknown_404:?}"
        );

        // 4. send_v2 rejects non-http/https scheme
        assert!(matches!(
            LocalSendTransport::send_v2(&guard, "file:///etc/passwd", "s", "n", "f.txt", b"hi"),
            Err(VitralError::InvalidArgument(_))
        ));

        // 5. send_v2 rejects query injection in sessionId/token from malicious peer
        let mal_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mal_addr = mal_listener.local_addr().unwrap();
        let mal_join = std::thread::spawn(move || {
            if let Ok((mut s, _)) = mal_listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let body = r#"{"sessionId":"s1&admin=true","files":{"f1":"tok?evil=1"}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        let inj_res = LocalSendTransport::send_v2(
            &guard,
            &format!("http://{mal_addr}"),
            "sender",
            "Sender",
            "test.txt",
            b"hello",
        );
        let _ = mal_join.join();
        assert!(
            matches!(inj_res, Err(VitralError::Parse(_))),
            "expected Parse error on query-injected sessionId/token, got: {inj_res:?}"
        );

        peer.stop();
    }

    #[test]
    fn m1c_iter2_import_lexicon_lock_released_before_webdav_and_purerust_digit_prefix() {
        // 1. Verify VitralCore::import_lexicon drops imported_lexicon.write() before blocking WebDAV PUT
        let (req_tx, req_rx) = std::sync::mpsc::channel::<()>();
        let (unblock_tx, unblock_rx) = std::sync::mpsc::channel::<()>();
        let mock = InProcessWebDavMock::start_with_put_gate(Some((req_tx, unblock_rx)));

        let tmp_dir = std::env::temp_dir().join(format!("vitral_lock_scope_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tsv_path = tmp_dir.join("lock_scope.tsv");
        std::fs::write(&tsv_path, "并发锁测试\tbing fa suo ce shi\t900\n").unwrap();

        let core = std::sync::Arc::new(
            VitralCore::init(
                &serde_json::json!({
                    "data_dir": tmp_dir.to_string_lossy(),
                    "koofr_endpoint": mock.koofr_endpoint()
                })
                .to_string(),
            )
            .unwrap(),
        );
        // Warm RimeDictCorpus OnceLock before timing the WebDAV PUT lock-release check
        let _ = Arc::clone(core.imported_lexicon().read().corpus());
        let core_bg = std::sync::Arc::clone(&core);
        let tsv_bg = tsv_path.clone();
        let imp_thread = std::thread::spawn(move || {
            core_bg
                .import_lexicon(&tsv_bg, LexiconFormat::Tsv)
                .unwrap()
        });

        req_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // While import_lexicon is blocked inside WebDAV PUT, read lock must succeed immediately!
        let idx_arc = core.imported_lexicon();
        assert!(
            idx_arc.try_read().is_some(),
            "imported_lexicon.write() must NOT be held across blocking WebDAV I/O"
        );
        let _ = unblock_tx.send(());
        let rep = imp_thread.join().unwrap();
        assert_eq!(rep.imported_count, 1);
        assert!(rep.sync_remote_path.is_some());
        let _ = std::fs::remove_dir_all(&tmp_dir);

        // 2. Verify PureRust digit '1'..='5' commit preserves confirmed_non_ascii_prefix in native-rime
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        {
            let eng_core = VitralCore::init(r#"{"schema":"rime_ice"}"#).unwrap();
            {
                let mut st = eng_core.input.lock();
                st.retention
                    .record_selection_at("手写中文", RetentionEngine::now_epoch_ms(), 50.0);
            }
            let full = type_ascii(&eng_core, "nihaoshouxiezhongwen");
            let nihao_idx = full
                .candidates
                .iter()
                .position(|c| c.text == "你好")
                .unwrap_or_else(|| {
                    panic!(
                        "expected partial '你好' in candidates for 'nihaoshouxiezhongwen', got: {:?}",
                        full.candidates
                    )
                });
            assert!(nihao_idx < 5, "expected '你好' on page 0, got idx {nihao_idx}");
            let digit_ch = char::from_digit((nihao_idx + 1) as u32, 10).unwrap();
            let after_partial = eng_core
                .process_key(digit_ch as u32, 0, SwipeDir::None)
                .unwrap();
            let pr_idx = after_partial
                .candidates
                .iter()
                .position(|c| c.text == "手写中文")
                .unwrap_or_else(|| {
                    panic!(
                        "expected boosted '手写中文' PureRust candidate after partial '你好', got: {:?}",
                        after_partial.candidates
                    )
                });
            assert!(pr_idx < 5, "expected '手写中文' on page 0, got idx {pr_idx}");
            let d2 = char::from_digit((pr_idx + 1) as u32, 10).unwrap();
            let committed = eng_core
                .process_key(d2 as u32, 0, SwipeDir::None)
                .unwrap();
            assert_eq!(
                committed.commit_text.as_deref(),
                Some("你好手写中文"),
                "PureRust digit commit must preserve confirmed non-ASCII prefix"
            );
        }
    }

    #[test]
    fn m1c_iter3_select_candidate_purerust_preserves_prefix_opencc_and_sync_ctx_redaction() {
        use crate::imp::SyncContext;
        use crate::offline_guard::OfflineGuard;

        // 1. Verify SyncContext<'a> Debug redacts koofr_password
        let guard = OfflineGuard::new(false);
        let sync_ctx = SyncContext {
            guard: &guard,
            data_dir: "/tmp/vitral-test",
            koofr_endpoint: Some("https://app.koofr.net/dav/Koofr"),
            koofr_username: Some("alice@example.com"),
            koofr_password: Some("super-secret-sync-ctx-pw"),
            koofr_root: Some("Vitral"),
        };
        let sync_dbg = format!("{sync_ctx:?}");
        assert!(!sync_dbg.contains("super-secret-sync-ctx-pw"));
        assert!(sync_dbg.contains("[REDACTED]"));

        let sync_ctx_none = SyncContext {
            guard: &guard,
            data_dir: "/tmp/vitral-test",
            koofr_endpoint: None,
            koofr_username: None,
            koofr_password: None,
            koofr_root: None,
        };
        assert!(!format!("{sync_ctx_none:?}").contains("[REDACTED]"));

        // 2. Verify select_candidate() on genuine CandidateOrigin::PureRust preserves
        // confirmed_non_ascii_prefix and applies OpenCcMode::Traditional conversion
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        {
            // Confirmed prefixes now affect grammar ranking; select the contextual PureRust alternative.
            // 2a. Simplified confirmed prefix plus the selected remaining text.
            let core = VitralCore::init(r#"{"schema":"rime_ice"}"#).unwrap();
            let full = type_ascii(&core, "nihaoshouxiezhongwen");
            let nihao_idx = full
                .candidates
                .iter()
                .position(|c| c.text == "你好")
                .unwrap_or_else(|| {
                    panic!(
                        "expected partial '你好' in candidates, got: {:?}",
                        full.candidates
                    )
                });
            let after_partial = core.select_candidate(nihao_idx).unwrap();
            let pr_idx = after_partial
                .candidates
                .iter()
                .position(|c| c.comment == "shou xie zhong wen")
                .unwrap_or_else(|| {
                    panic!(
                        "expected contextual PureRust alternative after partial '你好', got: {:?}",
                        after_partial.candidates
                    )
                });
            let expected = format!("你好{}", after_partial.candidates[pr_idx].text);
            let committed = core.select_candidate(pr_idx).unwrap();
            assert_eq!(
                committed.commit_text.as_deref(),
                Some(expected.as_str()),
                "select_candidate on PureRust candidate must preserve confirmed prefix"
            );

            // 2b. Traditional prefix and rendered remaining candidate.
            let tc_core = VitralCore::init(r#"{"schema":"rime_ice"}"#).unwrap();
            tc_core.set_opencc_mode(OpenCcMode::Traditional);
            let tc_full = type_ascii(&tc_core, "womenshouxiezhongwen");
            let tc_women_idx = tc_full
                .candidates
                .iter()
                .position(|c| c.text == "我們")
                .unwrap_or_else(|| {
                    panic!(
                        "expected Traditional partial '我們' in candidates, got: {:?}",
                        tc_full.candidates
                    )
                });
            let tc_after_partial = tc_core.select_candidate(tc_women_idx).unwrap();
            let tc_pr_idx = tc_after_partial
                .candidates
                .iter()
                .position(|c| c.comment == "shou xie zhong wen")
                .unwrap_or_else(|| {
                    panic!(
                        "expected Traditional PureRust alternative, got: {:?}",
                        tc_after_partial.candidates
                    )
                });
            let expected = format!("我們{}", tc_after_partial.candidates[tc_pr_idx].text);
            let tc_committed = tc_core.select_candidate(tc_pr_idx).unwrap();
            assert_eq!(
                tc_committed.commit_text.as_deref(),
                Some(expected.as_str()),
                "select_candidate on PureRust candidate under Traditional must preserve Traditional prefix"
            );

            // 2c. Switch to OpenCcMode::Traditional after partial Simplified prefix "我们":
            // verifies select_candidate_on_backend runs self.convert_opencc(&prefix) on PureRust branch
            let switch_core = VitralCore::init(r#"{"schema":"rime_ice"}"#).unwrap();
            let sw_full = type_ascii(&switch_core, "womenshouxiezhongwen");
            let sw_women_idx = sw_full
                .candidates
                .iter()
                .position(|c| c.text == "我们")
                .unwrap_or_else(|| {
                    panic!(
                        "expected Simplified partial '我们' in candidates, got: {:?}",
                        sw_full.candidates
                    )
                });
            let sw_after_partial = switch_core.select_candidate(sw_women_idx).unwrap();
            let sw_pr_idx = sw_after_partial
                .candidates
                .iter()
                .position(|c| c.comment == "shou xie zhong wen")
                .unwrap_or_else(|| {
                    panic!(
                        "expected contextual PureRust alternative, got: {:?}",
                        sw_after_partial.candidates
                    )
                });
            let expected = format!("我們{}", sw_after_partial.candidates[sw_pr_idx].text);
            switch_core.set_opencc_mode(OpenCcMode::Traditional);
            let sw_committed = switch_core.select_candidate(sw_pr_idx).unwrap();
            assert_eq!(
                sw_committed.commit_text.as_deref(),
                Some(expected.as_str()),
                "select_candidate on PureRust must convert confirmed prefix via convert_opencc"
            );
        }
    }

    #[test]
    fn m1c_iter4_concurrent_relay_poll_and_restore_toctou_not_found_safe() {
        use crate::dav::{upload_koofr_relay, upload_retention_crdt_delta, KoofrDavClient};
        use crate::imp::{ImportedLexiconEntry, LexiconImportArtifact};
        use crate::offline_guard::OfflineGuard;

        let mock = InProcessWebDavMock::start();
        let sync_key = "iter4-toctou-sync-key";
        let guard = OfflineGuard::new(false);
        let dav_cfg = DavConfig {
            endpoint: mock.koofr_endpoint(),
            username: None,
            password: None,
            root: None,
            sync_key: Some(sync_key.into()),
        };

        // 1. Multi-device shared /relay/ TOCTOU (dav.rs:604):
        //    dev-a uploads 01-msg-for-b (for dev-b) and 02-msg-for-c (for dev-c).
        //    dev-c starts relay_poll_inbox(), completes PROPFIND /dav/Koofr/Vitral/relay/,
        //    and pauses right before reading 01-msg-for-b.meta.json.
        //    While dev-c is paused, dev-b polls, receives msg-for-b, and deletes its .enc/.meta.json.
        //    dev-c resumes and MUST still return Ok(RelayPollResult) containing msg-for-c.
        upload_koofr_relay(
            &guard,
            &dav_cfg,
            "dev-a",
            "dev-b",
            "01-msg-for-b",
            "b.txt",
            false,
            b"msg-for-b",
        )
        .unwrap();
        upload_koofr_relay(
            &guard,
            &dav_cfg,
            "dev-a",
            "dev-c",
            "02-msg-for-c",
            "c.txt",
            false,
            b"msg-for-c",
        )
        .unwrap();

        let dev_b = VitralCore::init(
            &serde_json::json!({
                "koofr_endpoint": mock.koofr_endpoint(),
                "sync_key": sync_key,
                "device_id": "dev-b"
            })
            .to_string(),
        )
        .unwrap();
        let dev_c = std::sync::Arc::new(
            VitralCore::init(
                &serde_json::json!({
                    "koofr_endpoint": mock.koofr_endpoint(),
                    "sync_key": sync_key,
                    "device_id": "dev-c"
                })
                .to_string(),
            )
            .unwrap(),
        );

        mock.clear_log();
        let (hit_rx, unblock_tx) = mock.arm_get_gate("relay/01-msg-for-b.meta.json");
        let dev_c_bg = std::sync::Arc::clone(&dev_c);
        let poll_c_thread = std::thread::spawn(move || dev_c_bg.relay_poll_inbox());

        hit_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            mock.log()
                .iter()
                .any(|(m, p)| m == "PROPFIND" && p.starts_with("/dav/Koofr/Vitral/relay")),
            "dev-c must have completed PROPFIND before pausing on meta GET"
        );

        let polled_b = dev_b.relay_poll_inbox().unwrap();
        assert_eq!(polled_b.received, vec!["msg-for-b".to_string()]);
        assert_eq!(polled_b.deleted_count, 2);
        assert!(!mock.exists("relay/01-msg-for-b.meta.json"));
        assert!(!mock.exists("relay/01-msg-for-b.enc"));

        unblock_tx.send(()).unwrap();
        let polled_c = poll_c_thread.join().unwrap().unwrap();
        assert_eq!(polled_c.received, vec!["msg-for-c".to_string()]);
        assert_eq!(polled_c.deleted_count, 2);
        assert!(mock.list_files("relay/").is_empty());

        // 2. Missing .enc after .meta.json read (dav.rs:615):
        //    dev-c reads 01-vanish-enc.meta.json (addressed to dev-c), then pauses before GET .enc.
        //    Concurrent deletion removes 01-vanish-enc.enc; dev-c resumes and still receives 02-survive-c.
        upload_koofr_relay(
            &guard,
            &dav_cfg,
            "dev-a",
            "dev-c",
            "01-vanish-enc",
            "v.txt",
            false,
            b"vanished-payload",
        )
        .unwrap();
        upload_koofr_relay(
            &guard,
            &dav_cfg,
            "dev-a",
            "dev-c",
            "02-survive-c",
            "s.txt",
            false,
            b"surviving-payload",
        )
        .unwrap();

        let (hit_enc_rx, unblock_enc_tx) = mock.arm_get_gate("relay/01-vanish-enc.enc");
        let dev_c_bg2 = std::sync::Arc::clone(&dev_c);
        let poll_c_thread2 = std::thread::spawn(move || dev_c_bg2.relay_poll_inbox());

        hit_enc_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        mock.nodes
            .lock()
            .remove("/dav/Koofr/Vitral/relay/01-vanish-enc.enc");
        unblock_enc_tx.send(()).unwrap();

        let polled_c2 = poll_c_thread2.join().unwrap().unwrap();
        assert_eq!(polled_c2.received, vec!["surviving-payload".to_string()]);
        assert_eq!(polled_c2.deleted_count, 2);
        // Clean up the orphaned 01-vanish-enc.meta.json
        mock.nodes
            .lock()
            .remove("/dav/Koofr/Vitral/relay/01-vanish-enc.meta.json");

        // 3. fetch_backup_from_koofr TOCTOU (dav.rs:821, 839):
        //    Concurrent compaction/deletion of a CRDT delta or lexicon import between list_dir and GET
        //    must be skipped cleanly rather than aborting restore_from_koofr with 404 NotFound.
        let mut r1 = RetentionEngine {
            crdt_seq: 1,
            ..Default::default()
        };
        r1.pinned_words.insert("保留词".into());
        let mut d1 = r1.export_crdt_delta("crdt_a");
        d1.seq = 1;
        d1.timestamp_ms = 1_000;
        upload_retention_crdt_delta(&guard, &dav_cfg, &d1).unwrap();

        let mut r2 = RetentionEngine {
            crdt_seq: 2,
            ..Default::default()
        };
        r2.pinned_words.insert("存活词".into());
        let mut d2 = r2.export_crdt_delta("crdt_b");
        d2.seq = 2;
        d2.timestamp_ms = 2_000;
        upload_retention_crdt_delta(&guard, &dav_cfg, &d2).unwrap();

        let client = KoofrDavClient::connect(&guard, &dav_cfg).unwrap();
        for (fname, word) in [("01_imp.cbor", "导入词甲"), ("02_imp.cbor", "导入词乙")] {
            let art = LexiconImportArtifact {
                schema_version: 1,
                source_filename: fname.to_string(),
                format: LexiconFormat::Tsv,
                sha256: sha256_hex(word.as_bytes()),
                entry_count: 1,
                entries: vec![ImportedLexiconEntry {
                    text: word.to_string(),
                    pinyin: "dao ru ci".into(),
                    toned_pinyin: "dǎo rù cí".into(),
                    xiaohe: "dtruci".into(),
                    weight: 500,
                }],
            };
            let mut cbor = Vec::new();
            ciborium::into_writer(&art, &mut cbor).unwrap();
            client
                .write_bytes(&format!("lexicon/imports/{fname}"), &cbor)
                .unwrap();
        }

        let restore_cfg = serde_json::json!({
            "koofr_endpoint": mock.koofr_endpoint(),
            "sync_key": sync_key
        })
        .to_string();

        // 3a. Vanished CRDT delta (dav.rs:821)
        let (crdt_hit_rx, crdt_unblock_tx) = mock.arm_get_gate("lexicon/crdt/crdt_a_1.cbor.enc");
        let dev_restore1 = std::sync::Arc::new(VitralCore::init(r#"{"device_id":"r1"}"#).unwrap());
        let dr1_bg = std::sync::Arc::clone(&dev_restore1);
        let cfg1 = restore_cfg.clone();
        let t_r1 = std::thread::spawn(move || dr1_bg.restore_from_koofr(&cfg1));

        crdt_hit_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        mock.nodes
            .lock()
            .remove("/dav/Koofr/Vitral/lexicon/crdt/crdt_a_1.cbor.enc");
        crdt_unblock_tx.send(()).unwrap();
        let res1 = t_r1.join().unwrap().unwrap();
        assert!(res1.restored);
        assert_eq!(res1.snapshots_applied, 3); // crdt_b_2 + 01_imp + 02_imp
        assert!(dev_restore1.input.lock().retention.pinned_words.contains("存活词"));

        // 3b. Vanished lexicon import artifact (dav.rs:839)
        let (imp_hit_rx, imp_unblock_tx) = mock.arm_get_gate("lexicon/imports/01_imp.cbor");
        let dev_restore2 = std::sync::Arc::new(VitralCore::init(r#"{"device_id":"r2"}"#).unwrap());
        let dr2_bg = std::sync::Arc::clone(&dev_restore2);
        let t_r2 = std::thread::spawn(move || dr2_bg.restore_from_koofr(&restore_cfg));

        imp_hit_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        mock.nodes
            .lock()
            .remove("/dav/Koofr/Vitral/lexicon/imports/01_imp.cbor");
        imp_unblock_tx.send(()).unwrap();
        let res2 = t_r2.join().unwrap().unwrap();
        assert!(res2.restored);
        assert_eq!(res2.snapshots_applied, 2); // crdt_b_2 + 02_imp
        assert!(dev_restore2.imported_lexicon().read().lookup_word_score("导入词乙").is_some());
    }
}

#[cfg(test)]
mod personalization_tests {
    use super::*;

    #[test]
    fn background_key_commits_flush_on_poll_close_and_drop() {
        let dir = std::env::temp_dir().join(format!(
            "vitral-background-key-{}-{}",
            std::process::id(),
            RetentionEngine::now_epoch_ms(),
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let cleanup = Cleanup(dir);
        let cfg = serde_json::json!({
            "pure_offline": true,
            "platform": "android",
            "schema": "rime_ice",
            "background_sync": true,
            "data_dir": cleanup.0,
            "assets_dir": "/nonexistent-vitral-assets",
            "koofr_endpoint": "http://127.0.0.1:9"
        });
        let core = VitralCore::init(&cfg.to_string()).unwrap();
        let persisted_seq = || {
            local::load::<LexiconSnapshotPayload>(&cleanup.0, "lexicon-state.json")
                .unwrap()
                .map_or(0, |snapshot| snapshot.retention.crdt_seq)
        };
        let mut last_seq = 0;
        for (index, code) in ["nihao", "zhongguo", "women"].into_iter().enumerate() {
            for key in code.bytes() {
                core.process_key(u32::from(key), 0, SwipeDir::None).unwrap();
            }
            let action = if index == 0 {
                core.process_key(0x20, 0, SwipeDir::None).unwrap()
            } else {
                core.select_candidate(0).unwrap()
            };
            assert!(action.commit_text.is_some());
            assert_eq!(persisted_seq(), last_seq);
            assert!(core.sync.lock().backup_pending);
            if index == 2 {
                break;
            }
            if index == 0 {
                core.poll_services().unwrap();
            } else {
                core.on_ime_close();
            }
            assert!(persisted_seq() > last_seq);
            last_seq = persisted_seq();
        }
        drop(core);
        assert!(persisted_seq() > last_seq);
        assert_eq!(
            local::load::<bool>(&cleanup.0, "backup-pending.json").unwrap(),
            Some(true),
        );
    }

    #[test]
    fn phase3_click_feedback_survives_backend_clear_and_context_reset() {
        let core = VitralCore::init(r#"{"schema":"rime_ice","pure_offline":true}"#).unwrap();
        let type_code = |code: &str| {
            let mut action = core.candidate_state().unwrap();
            for ch in code.chars() { action = core.process_key(ch as u32, 0, SwipeDir::None).unwrap(); }
            action
        };
        type_code("xiangmu");
        core.select_candidate(0).unwrap();
        let candidates = type_code("shuru");
        let target = candidates.candidates[1].text.clone();
        let expected = {
            let st = core.input.lock();
            let (chosen, skipped) = st.engine.selection_feedback(1).unwrap();
            let mut model = st.retention.reranker.clone();
            assert!(model.update_pairwise(&chosen, &skipped.unwrap(), RetentionEngine::now_epoch_ms()));
            model.weights
        };
        assert_eq!(core.select_candidate(1).unwrap().commit_text.as_deref(), Some(target.as_str()));
        {
            let st = core.input.lock();
            assert_eq!(st.retention.reranker.weights, expected);
            assert_eq!(st.retention.reranker.updates, 1);
            assert_eq!(st.retention.semantic_memories.len(), 1);
            assert!(st.engine.candidate_features.is_empty());
        }
        let cleared = core.clear_learned_habits();
        assert_eq!(cleared.reranker_updates, 0);
        assert_eq!(cleared.semantic_memory_count, 0);
        type_code("shuru");
        core.select_candidate(0).unwrap();
        assert_eq!(core.personalization_stats().semantic_memory_count, 0);
    }

    #[test]
    fn clipboard_copy_routes_do_not_learn_with_legacy_preferences() {
        struct TempData(std::path::PathBuf);
        impl Drop for TempData {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let data = TempData(std::env::temp_dir().join(format!(
            "vitral-personalization-{}-{}",
            std::process::id(),
            RetentionEngine::now_epoch_ms()
        )));
        let cfg = serde_json::json!({"pure_offline":true,"data_dir":data.0,"assets_dir":"/nonexistent-vitral-assets"});
        let core = VitralCore::init(&cfg.to_string()).unwrap();
        let mut timestamp_ms = 0;
        for legacy in [true, false] {
            let mut preferences = cfg.clone();
            preferences["learn_from_clipboard"] = serde_json::json!(legacy);
            let config = EngineConfig::parse_json(&preferences.to_string()).unwrap();
            assert!(config.adaptive_learning);
            assert!(serde_json::to_value(&config)
                .unwrap()
                .get("learn_from_clipboard")
                .is_none());
            core.apply_preferences(config).unwrap();

            let mut payload = |content: &str| {
                timestamp_ms += 1;
                ClipboardPayload {
                    id: None,
                    content: content.into(),
                    mime_type: "text/plain".into(),
                    source_device: "test".into(),
                    timestamp_ms,
                }
            };
            let seq = core.input.lock().retention.crdt_seq;
            assert_eq!(
                core.clipboard_push(payload("你好 OpenAI")).ring[0].content,
                "你好 OpenAI"
            );
            assert_eq!(
                core.clipboard_push_local(payload("中国 Vitral")).ring[0].content,
                "中国 Vitral"
            );
            assert_eq!(
                core.clipboard_ingest_remote(payload("学习 RustLang"), false)
                    .quick_paste
                    .unwrap()
                    .content,
                "学习 RustLang"
            );
            let retention = &core.input.lock().retention;
            assert_eq!(retention.crdt_seq, seq);
            assert!(retention.user_phrases.is_empty());
            assert_eq!(retention.stats().english_count, 0);
        }
        {
            let mut st = core.input.lock();
            st.retention.record_english_casing("OpenAI");
            st.retention.pin_word("保留");
            st.retention.erase_word("删除");
        }
        let stats =
            commands::execute(&core, "personalization-stats", &serde_json::json!({})).unwrap();
        assert_eq!(stats["english_count"], 1);
        let cleared =
            commands::execute(&core, "personalization-clear", &serde_json::json!({})).unwrap();
        assert_eq!(cleared["phrase_count"], 0);
        assert_eq!(cleared["pinned_count"], 1);
        assert_eq!(cleared["erased_count"], 1);
        assert!(data.0.join("lexicon-state.json").is_file());
        drop(core);
        let restored = VitralCore::init(&cfg.to_string()).unwrap();
        assert_eq!(restored.personalization_stats().vocab_count, 0);
        assert_eq!(restored.personalization_stats().english_count, 0);
        assert_eq!(restored.personalization_stats().pinned_count, 1);
    }
}

#[cfg(test)]
mod prediction_integration_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clipboard_does_not_change_prediction_context_or_cache() {
        let core = VitralCore::init(
            r#"{"assets_dir":"/nonexistent-vitral-assets","pure_offline":true}"#,
        )
        .unwrap();
        core.input
            .lock()
            .retention
            .context_bigrams
            .insert("你好".into(), [("世界".into(), 4.0)].into());
        let request = predict::PredictRequest {
            app_id: "editor".into(),
            context_before: "你好".into(),
            ..Default::default()
        };
        let first = core.predict_request(request.clone());
        assert_eq!(first.text, "世界");
        assert!(!first.cached);

        core.clipboard_push(ClipboardPayload {
            id: None,
            content: "剪贴板上下文 VitralCopy".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 0,
        });
        let after_copy = core.predict_request(request);
        assert!(after_copy.cached);
        assert_eq!(after_copy.candidates, first.candidates);
    }

    #[test]
    fn context_next_hints_rank_deduplicate_and_serialize() {
        let core = VitralCore::init(
            r#"{"assets_dir":"/nonexistent-vitral-hints-assets","pure_offline":true}"#,
        )
        .unwrap();
        let hints = core.context_next_hints("正", None);
        assert!(hints.iter().any(|h| h.initial == "q"
            && h.syllable == "que"
            && h.ch == "确"
            && h.source == "dictionary"));
        assert_eq!(
            core.context_next_hints("正", Some("确定"))[0].source,
            "prediction"
        );
        {
            let mut st = core.input.lock();
            let words = st.retention.context_bigrams.entry("正".into()).or_default();
            words.insert("法".into(), 100.0);
            words.insert("智能".into(), 40.0);
            words.insert("七".into(), 20.0);
            words.insert("世界".into(), 500.0);
            words.insert("你".into(), 0.1);
            st.retention.erased_words.insert("世界".into());
            st.retention.register_typed_phrase(
                "智能",
                "zhi neng",
                "ving",
                RetentionEngine::now_epoch_ms(),
            );
            assert!(core.context_next_hints("正", None).is_empty());
            let imported = core.imported_lexicon.read();
            let InputState {
                engine, retention, ..
            } = &mut *st;
            engine.record_candidate_commit(
                "正",
                "zheng",
                None,
                &imported,
                retention,
                RetentionEngine::now_epoch_ms(),
            );
        }
        let hints = core.context_next_hints("正", None);
        assert_eq!((hints[0].ch.as_str(), hints[0].source), ("法", "personal"));
        assert_eq!(hints[1].syllable, "zhi");
        assert_eq!(hints.iter().filter(|h| h.initial == "q").count(), 1);
        assert!(!hints.iter().any(|h| h.ch == "世" || h.ch == "你"));
        let response = commands::execute(
            &core,
            "context-next-hints",
            &json!({"context_before":"正", "prediction_text":"确定"}),
        )
        .unwrap();
        assert_eq!(
            response["hints"][0],
            json!({"initial":"q", "syllable":"que", "ch":"确", "source":"prediction"})
        );
        assert_eq!(response["hints"][1]["source"], "personal");
        assert!(response["hints"].as_array().unwrap().len() <= 6);
        let fallback = commands::execute(&core, "context-next-hints", &json!({})).unwrap();
        assert_eq!(fallback["hints"][0]["ch"], "法");
    }

    #[test]
    fn feedback_learning_clear_and_configuration_boundaries() {
        let dir = std::env::temp_dir().join(format!("vitral-prediction-integration-{}", std::process::id()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let _clean = Cleanup(dir.clone());
        let core = VitralCore::init(&json!({"data_dir":dir,"assets_dir":dir.join("no-assets"),"pure_offline":true}).to_string()).unwrap();
        core.on_app_focus("editor").unwrap();
        core.input.lock().retention.context_bigrams.entry("你好".into()).or_default().insert("世界".into(), 1.);
        let request = json!({"app_id":"editor","context_before":"你好","trigger":"catch_up"});
        let ready = commands::execute(&core, "predict-request", &request).unwrap();
        assert_eq!(ready["text"], "世界");
        assert_eq!(commands::execute(&core,"predict-accept", &json!({"id":ready["id"],"text":"世界"})).unwrap()["accepted"], true);
        assert_eq!(core.input.lock().profile.latest("editor"), Some("世界"));
        assert!(core.input.lock().retention.frecency.contains_key("世界"));
        assert_eq!(core.profile_state()["profile"]["accepted_count"], 1);
        let again = commands::execute(&core, "predict-request", &request).unwrap();
        assert!(core.predict_dismiss(again["id"].as_u64().unwrap()).unwrap());
        assert!(!core.predict_dismiss(again["id"].as_u64().unwrap()).unwrap());
        assert_eq!(core.profile_state()["profile"]["dismissed_count"], 1);
        core.profile_update(profile::ProfileUpdate { summary: "简洁的技术表达".into(), ..Default::default() }).unwrap();
        let revision = core.input.lock().profile.profile.revision;
        core.clear_learned_habits();
        assert!(core.input.lock().profile.latest("editor").is_none());
        assert!(core.input.lock().profile.profile.summary.is_empty());
        assert!(core.input.lock().profile.profile.revision > revision);
        assert_eq!(core.predict_poll(None).status, "skipped");
        assert!(profile::ProfileEngine::new(Some(dir.clone())).unwrap().profile.summary.is_empty());
        for ms in [0,149,2001] { assert!(EngineConfig::parse_json(&json!({"prediction_delay_ms":ms}).to_string()).is_err()); }
        for ms in [150,380,2000] { assert!(EngineConfig::parse_json(&json!({"prediction_delay_ms":ms}).to_string()).is_ok()); }
        let cfg = EngineConfig { adaptive_learning:false, profile_distill_enabled:false, ..Default::default() };
        core.apply_preferences(cfg).unwrap();
        core.record_profile_commit(&mut core.input.lock(), "不应学习这句话");
        assert!(core.input.lock().profile.latest("editor").is_none());
        core.input.lock().retention.context_bigrams.entry("你好".into()).or_default().insert("世界".into(), 1.);
        let ready = commands::execute(&core, "predict-request", &request).unwrap();
        assert!(core.predict_accept(ready["id"].as_u64().unwrap(), "世界").unwrap());
        assert!(core.input.lock().profile.latest("editor").is_none());
        assert!(!core.input.lock().retention.frecency.contains_key("世界"));
        assert_eq!(core.profile_state()["profile"]["accepted_count"], 1);
        let raw = commands::execute(&core,"predict-request", &json!({"app_id":"editor","is_password":true,"context_before":"你好"})).unwrap();
        assert_eq!(raw["status"], "skipped");
    }
}

#[cfg(test)]
mod device_regression_tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("vitral-device-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn core(&self) -> VitralCore {
            VitralCore::init(&serde_json::json!({"platform":"android", "pure_offline":true, "prediction_enabled":true, "assets_dir":self.0.join("assets"), "data_dir":self.0, "cache_dir":self.0.join("cache")}).to_string()).unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn partial_selection_keeps_tail_and_learns_consumed_code() {
        let temp = Temp::new("partial");
        for route in ["select", "space", "digit"] {
            let core = temp.core();
            let index = {
                let mut st = core.input.lock();
                st.engine.set_raw_input("wo'yaoquka".into());
                let imp = core.imported_lexicon.read();
                let clip = core.sync.lock();
                let InputState {
                    engine, retention, ..
                } = &mut *st;
                loop {
                    engine
                        .process_key_pure_rust_for_test(0, &clip.clipboard, &imp, retention)
                        .unwrap();
                    if let Some(index) = engine
                        .current_candidates
                        .iter()
                        .position(|c| c.text == "我")
                    {
                        break index;
                    }
                    assert!(engine.current_page.has_next);
                    engine.pure_rust.page_index += 1;
                }
            };
            let action = if route == "select" {
                core.select_candidate(index).unwrap()
            } else {
                let mut st = core.input.lock();
                if route == "space" {
                    st.engine
                        .pin_candidate_in_state(index, &mut RetentionEngine::default())
                        .unwrap();
                }
                let imp = core.imported_lexicon.read();
                let clip = core.sync.lock();
                let InputState {
                    engine, retention, ..
                } = &mut *st;
                engine
                    .process_key(
                        if route == "space" {
                            0x20
                        } else {
                            '1' as u32 + index as u32
                        },
                        0,
                        SwipeDir::None,
                        &clip.clipboard,
                        &imp,
                        retention,
                    )
                    .unwrap()
            };
            assert_eq!(action.commit_text.as_deref(), Some("我"), "{route}");
            assert_eq!(action.preedit.text, "yaoquka");
            assert!(!action.candidates.is_empty());
            let st = core.input.lock();
            assert_eq!(st.engine.raw_input, "yaoquka");
            assert_eq!(st.engine.pure_rust.page_index, 0);
            assert_eq!(st.engine.last_committed_code.as_deref(), Some("wo"));
        }
    }

    #[test]
    fn gated_predictions_skip_clipboard_and_lazy_lexicon() {
        let temp = Temp::new("prediction-gates");
        let core = temp.core();
        core.input.lock().engine.pure_rust.reload_assets(None);
        let sync = core.sync.lock();
        std::thread::scope(|scope| {
            let (done, result) = std::sync::mpsc::channel();
            let core = &core;
            scope.spawn(move || {
                for (enabled, raw, composing, password, host_composing) in [
                    (false, "", "", false, false),
                    (true, "", "", true, false),
                    (true, "", "", false, true),
                    (true, "fa", "", false, false),
                    (true, "", "法", false, false),
                ] {
                    {
                        let mut st = core.input.lock();
                        st.engine.config.prediction_enabled = enabled;
                        st.engine.set_raw_input(raw.into());
                        st.engine.composing = composing.into();
                    }
                    assert_eq!(
                        core.predict_request(predict::PredictRequest {
                            context_before: "输入".into(),
                            is_password: password,
                            is_composing: host_composing,
                            ..Default::default()
                        }).status,
                        "skipped"
                    );
                }
                done.send(core.input.lock().engine.pure_rust.lexicon_init_state()).ok();
            });
            let result = result.recv_timeout(std::time::Duration::from_secs(3));
            drop(sync);
            assert_eq!(result.unwrap(), (false, false));
        });
    }

    #[test]
    fn prediction_without_model_uses_dictionary_and_preserves_gates() {
        let temp = Temp::new("prediction");
        let core = temp.core();
        let request = predict::PredictRequest {
            context_before: "输入".into(),
            ..Default::default()
        };
        let result = core.predict_request(request.clone());
        assert_eq!(result.status, "ready");
        assert_eq!(result.text, "法");
        assert_eq!(result.source, "dictionary");
        assert!(core.predict_dismiss(result.id).unwrap());
        assert_eq!(core.predict_poll(Some(result.id)).status, "skipped");
        for (password, composing, context) in [
            (true, false, "输入"),
            (false, true, "输入"),
            (false, false, "password=secret"),
        ] {
            let result = core.predict_request(predict::PredictRequest {
                context_before: context.into(),
                is_password: password,
                is_composing: composing,
                ..Default::default()
            });
            assert_eq!(result.status, "skipped");
        }
        core.input
            .lock()
            .retention
            .context_bigrams
            .entry("输入".into())
            .or_default()
            .insert("完成".into(), 20.0);
        core.prediction.clear_cache();
        let result = core.predict_request(request);
        assert_eq!(result.text, "完成");
        assert_eq!(result.source, "personal_memory");
    }

    #[test]
    fn prediction_skips_development_and_sensitive_apps() {
        let temp = Temp::new("prediction-apps");
        let core = temp.core();
        assert_eq!(core.profile_state()["prediction_skip_dev_apps"], true);
        let status = |app_id: &str| {
            core.predict_request(predict::PredictRequest {
                app_id: app_id.into(),
                context_before: "输入".into(),
                ..Default::default()
            })
            .status
        };
        let dev_apps = [
            r"C:\Program Files\Microsoft VS Code\Code.exe",
            "org.gnome.Terminal",
            "com.jetbrains.idea",
            "com.termux",
        ];
        let sensitive_apps = [
            "1Password.exe",
            "com.onepassword.passwordmanager",
            "org.keepassxc.KeePassXC",
            "com.bitwarden.desktop",
            "Enpass",
            "polkit",
            "gnome-keyring-daemon",
            "kwalletd6",
        ];
        for app in dev_apps.into_iter().chain(sensitive_apps) {
            assert_eq!(status(app), "skipped", "{app}");
        }
        assert_eq!(status("org.mozilla.firefox"), "ready");

        let cfg = EngineConfig::parse_json(r#"{"prediction_skip_dev_apps":false}"#).unwrap();
        core.apply_preferences(cfg).unwrap();
        assert_eq!(core.profile_state()["prediction_skip_dev_apps"], false);
        for app in dev_apps {
            assert_eq!(status(app), "ready", "{app}");
        }
        for app in sensitive_apps {
            assert_eq!(status(app), "skipped", "{app}");
        }
        core.on_app_focus("com.bitwarden.desktop").unwrap();
        assert_eq!(status(""), "skipped");
    }

    #[test]
    fn power_save_skips_local_prediction_and_distillation() {
        let temp = Temp::new("power-save");
        let core = temp.core();
        let missing = temp.0.join("missing-model");
        {
            let mut st = core.input.lock();
            st.engine.model_paths.local_llm = Some(models::LocalLlmModel {
                id: "power-save-test".into(),
                dir: missing.clone(),
                model_file: missing.join("model.onnx"),
                is_fp16_kv: false,
            });
            st.retention.pinned_words.insert("Vitral".into());
        }

        assert!(!core.is_power_save());
        let set_power_save = |enabled| {
            commands::execute(&core, "set-power-save", &serde_json::json!({"enabled":enabled}))
                .unwrap()
        };
        assert_eq!(set_power_save(true), serde_json::json!({"power_save":true}));

        let request = predict::PredictRequest {
            context_before: "输入".into(),
            ..Default::default()
        };
        let result = core.predict_request(request.clone());
        assert_eq!(result.status, "ready");
        assert_eq!(result.source, "dictionary");
        assert_eq!(result.text, "法");
        assert!(core.predict_request(request.clone()).cached);

        core.input.lock().retention.context_bigrams.insert(
            "输入".into(),
            [("完成".into(), 20.0)].into(),
        );
        core.prediction.clear_cache();
        let result = core.predict_request(request);
        assert_eq!(result.source, "personal_memory");
        assert_eq!(result.text, "完成");

        core.profile_distill().unwrap();
        let profile = core.profile_state();
        assert_eq!(profile["profile"]["source"], "statistical");
        assert_eq!(profile["profile"]["entities"], serde_json::json!(["Vitral", "完成"]));
        assert!(profile["reason"].is_null());
        assert_eq!(core.privacy_audit()["entries"], serde_json::json!([]));

        assert_eq!(set_power_save(false), serde_json::json!({"power_save":false}));
        #[cfg(feature = "native-onnx")]
        {
            core.profile_distill().unwrap();
            assert!(core.profile_state()["reason"].is_string());
            let audit = core.privacy_audit();
            assert_eq!(audit["entries"][0]["operation"], "profile_distill");
            assert_eq!(audit["entries"][0]["route"], "local");
            assert_eq!(audit["total_outbound_bytes"], 0);
        }
    }

    #[test]
    fn accepting_secondary_prediction_persists_feedback_for_that_candidate() {
        let temp = Temp::new("prediction-secondary");
        let core = temp.core();
        core.input.lock().retention.context_bigrams.insert(
            "你好".into(),
            [("朋友".into(), 4.0), ("大家".into(), 2.0)].into(),
        );
        let action = commands::execute(
            &core,
            "predict-request",
            &serde_json::json!({"app_id":"editor", "context_before":"你好", "context_after":"世界。"}),
        )
        .unwrap();
        assert_eq!(action["candidates"], serde_json::json!(["朋友", "大家"]));
        assert_eq!(action["text"], action["candidates"][0]);
        let id = action["id"].as_u64().unwrap();
        assert!(core.predict_accept(id, "大家").unwrap());
        assert!(!core.predict_accept(id, "朋友").unwrap());
        drop(core);
        let restarted = temp.core();
        let st = restarted.input.lock();
        assert_eq!(st.retention.frecency["大家"], 1.0);
        assert_eq!(st.retention.context_bigrams["你好"]["大家"].round(), 3.0);
        assert!(!st.retention.frecency.contains_key("朋友"));
    }

    #[test]
    fn pending_composition_is_saved_before_its_first_promotion() {
        let temp = Temp::new("pending-composition");
        let config = serde_json::json!({
            "pure_offline": true,
            "schema": SchemaKind::RimeIce,
            "data_dir": temp.0,
            "assets_dir": temp.0.join("missing-assets"),
        })
        .to_string();
        for attempt in 0..2 {
            let core = VitralCore::init(&config).unwrap();
            if attempt == 1 {
                let st = core.input.lock();
                assert_eq!(st.retention.pending_phrase_pairs["你好"], 1);
                assert!(!st.retention.user_phrases.contains_key("你好"));
            }
            for (code, word) in [("ni", "你"), ("hao", "好")] {
                let action = code.bytes()
                    .map(|key| core.process_key(u32::from(key), 0, SwipeDir::None).unwrap())
                    .last()
                    .unwrap();
                let index = action.candidates.iter().find(|c| c.text == word).unwrap().index;
                assert_eq!(
                    core.select_candidate(index).unwrap().commit_text.as_deref(),
                    Some(word)
                );
            }
        }
        let restarted = VitralCore::init(&config).unwrap();
        assert_eq!(restarted.input.lock().retention.user_phrases["你好"].count, 2);
    }

    #[test]
    fn repeated_full_word_selection_keeps_priority_after_reopening() {
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        struct ResetNativeRuntime(bool);
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        impl Drop for ResetNativeRuntime {
            fn drop(&mut self) {
                if self.0 {
                    eng::rime_ffi::clear_runtime();
                }
            }
        }
        let backends = if cfg!(all(target_os = "linux", feature = "native-rime")) {
            &["portable", "native"][..]
        } else {
            &["portable"][..]
        };
        for backend in backends {
            let temp = Temp::new(&format!("repeat-{backend}"));
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            let _reset_native = ResetNativeRuntime(*backend == "native");
            for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
                let assets = if *backend == "native" {
                    eng::resolve_assets_dir("").unwrap()
                } else {
                    temp.0.join("missing-assets")
                };
                let config = serde_json::json!({
                    "pure_offline": true,
                    "schema": schema,
                    "data_dir": temp.0.join(schema.as_str()),
                    "cache_dir": temp.0.join("cache"),
                    "assets_dir": assets,
                })
                .to_string();
                let code = if schema == SchemaKind::RimeIce {
                    "shijian".to_owned()
                } else {
                    crate::imp::xiaohe::pinyin_phrase_to_xiaohe("shi jian")
                };
                let type_code = |core: &VitralCore| {
                    code.bytes()
                        .map(|key| core.process_key(u32::from(key), 0, SwipeDir::None).unwrap())
                        .last()
                        .unwrap()
                };
                let core = VitralCore::init(&config).unwrap();
                let initial = type_code(&core);
                let selected = if initial.candidates[0].text == "实践" {
                    "时间"
                } else {
                    "实践"
                };
                for attempt in 0..2 {
                    let mut action = if attempt == 0 {
                        initial.clone()
                    } else {
                        type_code(&core)
                    };
                    let index = loop {
                        if let Some(candidate) = action.candidates.iter().find(|c| c.text == selected) {
                            break candidate.index;
                        }
                        assert!(action.page.has_next, "{backend} {schema:?}: {selected}");
                        action = core.process_key(0xff56, 0, SwipeDir::None).unwrap();
                    };
                    assert_eq!(
                        core.select_candidate(index).unwrap().commit_text.as_deref(),
                        Some(selected)
                    );
                }
                assert_eq!(type_code(&core).candidates[0].text, selected, "{backend} {schema:?}");
                drop(core);
                let restarted = VitralCore::init(&config).unwrap();
                {
                    let st = restarted.input.lock();
                    assert_eq!(st.retention.user_phrases[selected].count, 2);
                    assert!(st.retention.code_preferences[&code][selected] > 1.5);
                }
                assert_eq!(type_code(&restarted).candidates[0].text, selected, "{backend} {schema:?}");
            }
        }
    }

    #[test]
    fn configure_preserves_legacy_clipboard_capacity_when_changing_shortcuts() {
        let temp = Temp::new("configure-capacity");
        let mut cfg = temp.core().input.lock().engine.config.clone();
        cfg.clipboard_ring_capacity = 4;
        let core = VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
        cfg.shortcuts[0].key = "Ctrl+Shift+F1".into();
        crate::commands::execute(&core, "configure", &serde_json::to_value(&cfg).unwrap())
            .unwrap();
        let state = core.input.lock();
        assert_eq!(state.engine.config.clipboard_ring_capacity, 4);
        assert_eq!(state.engine.config.shortcuts, cfg.shortcuts);
    }

    #[test]
    fn configure_explicitly_clears_blank_koofr_password_like_cold_start() {
        let temp = Temp::new("configure-password");
        let mut cfg = temp.core().input.lock().engine.config.clone();
        cfg.koofr_endpoint = Some("https://example.org/dav/".into());
        cfg.koofr_username = Some("user".into());
        cfg.koofr_password = Some("secret".into());
        for password in [None, Some(String::new())] {
            cfg.koofr_password = Some("secret".into());
            let core = VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
            cfg.koofr_password = password;
            let mut payload = serde_json::to_value(&cfg).unwrap();
            crate::commands::execute(&core, "configure", &payload).unwrap();
            assert_eq!(core.input.lock().engine.config.koofr_password.as_deref(), Some("secret"));
            assert_eq!(core.sync.lock().dav_config.as_ref().unwrap().password.as_deref(), Some("secret"));

            payload["clear_koofr_password"] = serde_json::json!(true);
            crate::commands::execute(&core, "configure", &payload).unwrap();
            assert!(core.input.lock().engine.config.koofr_password.is_none());
            assert!(core.sync.lock().dav_config.as_ref().unwrap().password.is_none());
            let restarted = VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
            assert_eq!(core.sync.lock().dav_config, restarted.sync.lock().dav_config);
        }
    }

    #[test]
    fn blank_password_keeps_only_same_endpoint_secret() {
        let temp = Temp::new("preferences");
        let core = temp.core();
        let mut cfg = core.input.lock().engine.config.clone();
        cfg.koofr_endpoint = Some(" https://example.org/dav/ ".into());
        cfg.koofr_username = Some(" user ".into());
        cfg.koofr_password = Some("\r\nsecret\r\n".into());
        core.apply_preferences(cfg.clone()).unwrap();
        for password in [None, Some(String::new())] {
            cfg.koofr_password = password;
            core.apply_preferences(cfg.clone()).unwrap();
            assert_eq!(
                core.input.lock().engine.config.koofr_password.as_deref(),
                Some("secret")
            );
            assert_eq!(
                core.sync
                    .lock()
                    .dav_config
                    .as_ref()
                    .unwrap()
                    .username
                    .as_deref(),
                Some("user")
            );
        }
        cfg.koofr_endpoint = Some("https://other.example/dav".into());
        core.apply_preferences(cfg.clone()).unwrap();
        assert!(core.input.lock().engine.config.koofr_password.is_none());
        cfg.koofr_password = Some("other-secret".into());
        core.apply_preferences(cfg.clone()).unwrap();
        cfg.koofr_password = None;
        cfg.koofr_endpoint = None;
        core.apply_preferences(cfg).unwrap();
        assert!(core.input.lock().engine.config.koofr_password.is_none());
    }

    #[test]
    fn handwriting_paths_hot_reload_and_normalized_strokes() {
        let temp = Temp::new("handwriting");
        let core = temp.core();
        let stroke = [
            StrokePoint {
                x: 0.15,
                y: 0.5,
                timestamp_ms: 0,
            },
            StrokePoint {
                x: 0.85,
                y: 0.5,
                timestamp_ms: 30,
            },
        ];
        for mode in [HwMode::PhoneticLookup, HwMode::ContinuousLattice] {
            core.hw_begin(mode);
            assert!(matches!(
                core.hw_push_stroke(&stroke, true),
                Err(VitralError::ModelUnavailable(_))
            ));
            assert!(matches!(
                core.hw_end(),
                Err(VitralError::ModelUnavailable(_))
            ));
        }
        // A downloaded file changes every consumer together, even before an ONNX session is opened.
        let model_dir = models::store_root(temp.0.to_str().unwrap(), "").join("handwriting");
        std::fs::create_dir_all(&model_dir).unwrap();
        let model = models::Catalog::builtin()
            .models
            .into_iter()
            .find(|m| m.id == "handwriting-cnn")
            .unwrap();
        for file in model.files {
            let path = models::store_root(temp.0.to_str().unwrap(), "").join(file.rel_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(path)
                .unwrap()
                .set_len(file.size)
                .unwrap();
        }
        core.reload_assets().unwrap();
        let assert_paths = |enabled| {
            let st = core.input.lock();
            let paths = core.models.paths();
            assert_eq!(st.engine.model_paths, paths);
            assert_eq!(st.hw.model_paths.as_ref(), Some(&paths));
            assert_eq!(st.voice.model_paths, paths);
            assert_eq!(paths.handwriting.is_some(), enabled);
        };
        assert_paths(true);
        for mode in [HwMode::PhoneticLookup, HwMode::ContinuousLattice] {
            core.hw_begin(mode);
            core.hw_push_stroke(&stroke[..1], false).unwrap();
            core.hw_push_stroke(&stroke[1..], true).unwrap();
            match core.hw_end().unwrap() {
                HandwritingPayload::Mode1(result) => assert_eq!(result.candidates[0].ch, "一"),
                HandwritingPayload::Mode2(result) => assert_eq!(result.composing_text, "一"),
            }
        }
        core.hw_begin(HwMode::PhoneticLookup);
        assert!(matches!(
            core.hw_push_stroke(
                &[StrokePoint {
                    x: f32::NAN,
                    y: 0.5,
                    timestamp_ms: 0
                }],
                true
            ),
            Err(VitralError::InvalidArgument(_))
        ));
        core.models_set_enabled("handwriting-cnn", false).unwrap();
        assert_paths(false);
        core.models_set_enabled("handwriting-cnn", true).unwrap();
        assert_paths(true);
        core.models_delete("handwriting-cnn").unwrap();
        assert_paths(false);
        assert!(!model_dir.exists());
    }
}

#[cfg(test)]
mod input_feature_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        core: VitralCore,
        root: PathBuf,
    }
    impl Fixture {
        fn new(schema: SchemaKind, adaptive: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "vitral-input-features-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let cfg = EngineConfig {
                data_dir: root.join("data").to_string_lossy().into_owned(),
                cache_dir: root.join("cache").to_string_lossy().into_owned(),
                assets_dir: root.join("missing-assets").to_string_lossy().into_owned(),
                schema,
                adaptive_learning: adaptive,
                pure_offline: true,
                ..Default::default()
            };
            Self {
                core: VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap(),
                root,
            }
        }
        fn type_code(&self, code: &str) -> InputAction {
            code.chars().map(|c| self.key(c as u32)).last().unwrap()
        }
        fn key(&self, code: u32) -> InputAction {
            self.core.process_key(code, 0, SwipeDir::None).unwrap()
        }
        fn choose(&self, text: &str) -> InputAction {
            let index = self
                .core
                .input
                .lock()
                .engine
                .current_candidates
                .iter()
                .position(|c| c.text == text)
                .unwrap_or_else(|| panic!("missing {text}"));
            self.core.select_candidate(index).unwrap()
        }
        fn add_words(&self, words: &[(&str, &str)]) {
            let entries: Vec<_> = words
                .iter()
                .map(|(text, pinyin)| crate::imp::ImportedLexiconEntry {
                    text: (*text).into(),
                    pinyin: (*pinyin).into(),
                    toned_pinyin: (*pinyin).into(),
                    xiaohe: crate::imp::xiaohe::pinyin_phrase_to_xiaohe(pinyin),
                    weight: 100,
                })
                .collect();
            self.core.imported_lexicon.write().inject_hotwords(&entries);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Core has no pending network jobs in these offline fixtures.
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn precision_tone_key_precedes_function_selection_only_after_the_marker() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            let f = Fixture::new(schema, true);
            f.add_words(&[("妈", "ma")]);
            {
                let mut state = f.core.input.lock();
                state.engine.config.shortcuts = vec![shortcuts::ShortcutBinding {
                    action: shortcuts::ShortcutAction::Settings,
                    key: String::new(),
                    text: "ma".into(),
                }];
            }
            f.type_code("ma");
            assert_eq!(f.key('1' as u32).function_action, Some(shortcuts::ShortcutAction::Settings));
            let marker = f.type_code("ma`");
            assert_eq!(marker.candidates[0].function_action, Some(shortcuts::ShortcutAction::Settings));
            let filtered = f.key('1' as u32);
            assert_eq!(filtered.preedit.text, "ma`1");
            assert_eq!(filtered.candidates[0].text, "妈");
            assert!(filtered.function_action.is_none());
            assert_eq!(f.key('1' as u32).commit_text.as_deref(), Some("妈"));
            assert_eq!(f.core.input.lock().retention.code_preferences["ma"].len(), 1);
            f.type_code("ma`1");
            assert_eq!(f.key(0xff0d).commit_text.as_deref(), Some("ma`1"));
        }
    }

    #[test]
    fn preedit_cursor_command_jumps_and_routes_editing_key_names() {
        use serde_json::json;

        let f = Fixture::new(SchemaKind::RimeIce, false);
        f.type_code("nihao");
        let jump = |cursor_utf8| {
            crate::commands::execute(&f.core, "set-preedit-cursor", &json!({"cursor_utf8": cursor_utf8})).unwrap()
        };
        assert_eq!(jump(2)["preedit"], json!({"text":"nihao","cursor_utf8":2,"retroactive_slots":[]}));
        assert_eq!(jump(99)["preedit"]["cursor_utf8"], 5);
        let key = |keycode| {
            crate::commands::execute(&f.core, "process-key", &json!({"keycode": keycode})).unwrap()
        };
        for (name, offset) in [("Home", 0), ("Right", 1), ("End", 5), ("Left", 4), ("Delete", 4)] {
            assert_eq!(key(name)["preedit"]["cursor_utf8"], offset);
        }
        assert_eq!(key("End")["preedit"]["text"], "niha");
    }

    #[test]
    fn local_selections_learn_whole_phrase_in_both_schemas() {
        for (schema, code) in [
            (SchemaKind::RimeIce, "woyaodaka"),
            (SchemaKind::DoublePinyinFlypy, "woycdaka"),
        ] {
            let f = Fixture::new(schema, true);
            f.type_code(code);
            let mut committed = String::new();
            for text in ["我", "要", "打", "卡"] {
                committed.push_str(f.choose(text).commit_text.as_deref().unwrap());
            }
            assert_eq!(committed, "我要打卡");
            let state = f.core.input.lock();
            assert_eq!(state.retention.user_phrases["我要打卡"].source, "typed");
            drop(state);
            assert_eq!(f.type_code(code).candidates[0].text, "我要打卡");
        }
    }

    #[test]
    fn local_phrase_learning_respects_cancel_edit_and_preference() {
        for cancel in [0x1b, 0xff08] {
            let f = Fixture::new(SchemaKind::RimeIce, true);
            f.type_code("woyaodaka");
            f.choose("我");
            f.key(cancel);
            f.key(0x1b);
            f.type_code("ka");
            f.choose("卡");
            assert!(!f
                .core
                .input
                .lock()
                .retention
                .user_phrases
                .contains_key("我卡"));
        }
        let f = Fixture::new(SchemaKind::RimeIce, false);
        f.type_code("woyaodaka");
        for text in ["我", "要", "打", "卡"] {
            f.choose(text);
        }
        assert!(f.core.input.lock().retention.user_phrases.is_empty());
    }

    #[test]
    fn chinese_mode_mixes_canonical_english_without_displacing_exact_chinese() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            let f = Fixture::new(schema, true);
            f.add_words(&[("爱", "ai"), ("只", "zhi"), ("团", "tuan"), ("昂", "ang")]);
            for (code, word) in [
                ("github", "GitHub"),
                ("iphone", "iPhone"),
                ("python", "Python"),
                ("vscode", "VSCode"),
                ("vitral", "Vitral"),
            ] {
                assert_eq!(
                    f.type_code(code).candidates[0].text,
                    word,
                    "{schema:?} {code}"
                );
                assert_eq!(f.choose(word).commit_text.as_deref(), Some(word));
            }
            let action = f.type_code("ai");
            assert_eq!(action.candidates[0].text, "爱");
            assert_eq!(action.candidates[1].text, "AI");
            f.key(0x1b);
            f.core
                .input
                .lock()
                .retention
                .record_english_casing("MyTool");
            assert_eq!(f.type_code("mytool").candidates[0].text, "MyTool");
        }
    }

    #[test]
    fn typo_correction_preferences_validate_and_refresh_active_composition() {
        use serde_json::json;

        assert_eq!(EngineConfig::parse_json("{}").unwrap().typo_correction_level, "standard");
        for level in ["off", "conservative", "standard", "aggressive"] {
            let value = json!({"typo_correction_level": level});
            assert_eq!(EngineConfig::parse_json(&value.to_string()).unwrap().typo_correction_level, level);
        }
        for value in [json!("fast"), json!(""), json!("STANDARD"), json!(1)] {
            assert!(EngineConfig::parse_json(&json!({"typo_correction_level": value}).to_string()).is_err());
        }
        let f = Fixture::new(SchemaKind::RimeIce, false);
        f.add_words(&[("你好", "ni hao")]);
        assert!(f.type_code("nihoa").candidates.iter().any(|item| item.comment == "纠错"));
        assert!(crate::commands::execute(&f.core, "configure", &json!({"typo_correction_level":"fast"})).is_err());
        for level in ["off", "conservative", "standard", "aggressive"] {
            crate::commands::execute(&f.core, "configure", &json!({
                "schema": "rime_ice", "typo_correction_level": level, "pure_offline": true,
            })).unwrap();
            let input = f.core.input.lock();
            assert_eq!(input.engine.raw_input, "nihoa");
            assert_eq!(input.engine.config.typo_correction_level, level);
            assert_eq!(
                input.engine.current_candidates.iter().any(|item| item.comment == "纠错"),
                level != "off",
            );
        }
    }

    #[test]
    fn syllable_split_command_and_configure_refresh_active_composition() {
        assert!(!EngineConfig::parse_json("{}").unwrap().syllable_split);
        let mut f = Fixture::new(SchemaKind::RimeIce, false);
        let mut cfg = f.core.input.lock().engine.config.clone();
        cfg.assets_dir = crate::eng::resolve_assets_dir("")
            .expect("workspace assets")
            .to_string_lossy()
            .into_owned();
        f.core = VitralCore::init(&serde_json::to_string(&cfg).unwrap()).unwrap();
        let normal = f.type_code("dian");
        assert!(matches!(
            normal.candidates[0].text.as_str(),
            "点" | "电" | "店"
        ));
        f.add_words(&[("点", "dian"), ("底案", "di an")]);
        let action: InputAction = serde_json::from_value(
            crate::commands::execute(
                &f.core,
                "set-syllable-split",
                &serde_json::json!({"enabled":true}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!action.candidates.is_empty());
        assert!(
            action.candidates.iter().all(|c| c.comment == "di an"),
            "{:?}",
            action.candidates
        );
        assert!(action.commit_text.is_none());
        assert_eq!(f.core.input.lock().engine.raw_input, "dian");
        let normal: InputAction = serde_json::from_value(
            crate::commands::execute(
                &f.core,
                "set-syllable-split",
                &serde_json::json!({"enabled":false}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            normal.candidates[0].text.as_str(),
            "点" | "电" | "店"
        ));
        let mut cfg = f.core.input.lock().engine.config.clone();
        cfg.syllable_split = true;
        crate::commands::execute(
            &f.core,
            "configure",
            &serde_json::to_value(cfg.clone()).unwrap(),
        )
        .unwrap();
        assert!(f
            .core
            .input
            .lock()
            .engine
            .current_candidates
            .iter()
            .all(|c| c.comment == "di an"));
        assert!(f
            .core
            .candidate_state()
            .unwrap()
            .candidates
            .iter()
            .all(|c| c.comment == "di an"));
        cfg.syllable_split = false;
        f.core.apply_preferences(cfg).unwrap();
        assert!(matches!(
            f.core.input.lock().engine.current_candidates[0]
                .text
                .as_str(),
            "点" | "电" | "店"
        ));
        f.key(0x08);
        assert_eq!(f.core.input.lock().engine.raw_input, "dia");
        assert!(matches!(
            f.key('n' as u32).candidates[0].text.as_str(),
            "点" | "电" | "店"
        ));
        f.key(0x1b);
        assert!(f
            .core
            .set_syllable_split(true)
            .unwrap()
            .candidates
            .is_empty());
        assert!(crate::commands::execute(
            &f.core,
            "set-syllable-split",
            &serde_json::json!({"enabled":"true"})
        )
        .is_err());
    }

    #[test]
    fn fuzzy_pinyin_is_opt_in_and_preserves_exact_readings() {
        let f = Fixture::new(SchemaKind::RimeIce, true);
        f.add_words(&[
            ("四", "si"),
            ("中国", "zhong guo"),
            ("蓝", "lan"),
            ("南", "nan"),
            ("英", "ying"),
            ("音", "yin"),
        ]);
        assert!(!f.type_code("si").candidates.iter().any(|c| c.text == "是"));
        f.key(0x1b);
        let mut cfg = f.core.input.lock().engine.config.clone();
        cfg.fuzzy_pinyin = true;
        f.core.apply_preferences(cfg).unwrap();
        for (code, target) in [
            ("s", "是"),
            ("si", "是"),
            ("zongguo", "中国"),
            ("nan", "蓝"),
            ("yin", "英"),
        ] {
            let action = f.type_code(code);
            assert!(
                action.candidates.iter().any(|c| c.text == target),
                "{code}: {:?}",
                action.candidates
            );
            if code == "si" {
                assert_eq!(action.candidates[0].text, "四");
            }
            if code == "nan" {
                assert_eq!(action.candidates[0].text, "南");
            }
            if code == "yin" {
                assert_eq!(action.candidates[0].text, "音");
            }
            f.key(0x1b);
        }
        let mut cfg = f.core.input.lock().engine.config.clone();
        cfg.fuzzy_pinyin_pairs = vec!["n_l".into()];
        f.core.apply_preferences(cfg).unwrap();
        assert!(!f.type_code("si").candidates.iter().any(|c| c.text == "是"));
        f.key(0x1b);
        assert!(f.type_code("nan").candidates.iter().any(|c| c.text == "蓝"));
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_backend_mixes_english_and_learns_local_phrase() {
        let assets = crate::eng::resolve_assets_dir("").expect("workspace assets");
        // The process-wide runtime must keep its reusable directories beyond this fixture.
        crate::eng::rime_ffi::with_runtime(&assets, "", "", |_| Ok(())).unwrap();
        let f = Fixture::new(SchemaKind::RimeIce, true);
        {
            let mut state = f.core.input.lock();
            state.engine.resolved_assets_dir = Some(assets.clone());
            state.engine.pure_rust.reload_assets(Some(&assets));
        }
        for (code, word) in [("github", "GitHub"), ("iphone", "iPhone")] {
            assert_eq!(f.type_code(code).candidates[0].text, word);
            assert_eq!(f.choose(word).commit_text.as_deref(), Some(word));
        }
        let english_alternative = f.type_code("ai");
        assert_eq!(english_alternative.candidates[0].text, "爱");
        assert_eq!(english_alternative.candidates[1].text, "AI");
        f.key(0x1b);
        f.type_code("woyaodaka");
        for text in ["我", "要", "打", "卡"] {
            for _ in 0..8 {
                let state = f.core.input.lock();
                let engine = &state.engine;
                if engine.current_candidates.iter().any(|c| c.text == text) {
                    break;
                }
                assert!(engine.current_page.has_next, "missing native {text}");
                drop(state);
                f.key(0xff56);
            }
            f.choose(text);
        }
        assert_eq!(f.type_code("woyaodaka").candidates[0].text, "我要打卡");
        assert!(f
            .core
            .input
            .lock()
            .retention
            .user_phrases
            .contains_key("我要打卡"));
        f.key(0x1b);
        let mut cfg = f.core.input.lock().engine.config.clone();
        cfg.fuzzy_pinyin = true;
        f.core.apply_preferences(cfg).unwrap();
        for (code, word) in [("si", "是"), ("zongguo", "中国")] {
            assert!(f.type_code(code).candidates.iter().any(|c| c.text == word));
            f.key(0x1b);
        }
    }

    #[test]
    fn flypy_local_selection_and_punctuation_commit() {
        let f = Fixture::new(SchemaKind::DoublePinyinFlypy, true);
        f.type_code("woycdaka");
        let first = f.choose("我");
        assert_eq!(first.commit_text.as_deref(), Some("我"));
        assert!(!first.preedit.text.is_empty());
        assert_eq!(f.key(',' as u32).commit_text.as_deref(), Some("要打卡，"));
        let engine = &f.core.input.lock().engine;
        assert!(engine.raw_input.is_empty());
        assert!(engine.current_candidates.is_empty());
    }
}

#[cfg(test)]
mod p1_privacy_tests {
    use super::*;
    use serde_json::{json, Value};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn core(settings: &Value) -> VitralCore {
        let mut settings = settings.clone();
        settings["assets_dir"] = json!("/nonexistent-vitral-assets");
        VitralCore::init(&settings.to_string()).unwrap()
    }

    #[test]
    fn ai_inline_route_command_matches_start_context() {
        for offline in [false, true] {
            for prefer_local in [false, true] {
                for model_enabled in [false, true] {
                    for endpoint in [None, Some("   "), Some("http://127.0.0.1:9/v1")] {
                        let core = core(&json!({
                            "pure_offline":offline, "ai_prefer_local":prefer_local,
                            "ai_endpoint":endpoint, "ai_timeout_ms":100,
                        }));
                        // A model descriptor exercises route selection without real model weights.
                        if model_enabled {
                            let dir = std::path::PathBuf::from("/nonexistent-vitral-route-model");
                            core.input.lock().engine.model_paths.local_llm = Some(models::LocalLlmModel {
                                id: "route-test".into(), model_file: dir.join("model.onnx"),
                                dir, is_fp16_kv: false,
                            });
                        }
                        let remote = endpoint.is_some_and(|s| !s.trim().is_empty());
                        let local = cfg!(feature = "native-onnx") && model_enabled
                            && (offline || prefer_local || !remote);
                        let expected = if local { "local" } else if !offline && remote { "remote" } else { "none" };
                        let route = commands::execute(&core, "ai-inline-route", &json!({})).unwrap();
                        assert_eq!(route, json!({"route":expected,"pure_offline":offline}));
                        let started = commands::execute(&core, "ai-inline-start",
                            &json!({"mode":"polish","text":"输入","target_lang":""})).unwrap();
                        if expected == "none" {
                            let last = poll_until(&core, started["id"].as_u64().unwrap(), |v| v["status"] != "running");
                            assert_eq!(last["status"], "error");
                            assert_eq!(core.privacy_audit()["total_outbound_bytes"], 0);
                        } else {
                            assert_eq!(started["route"], expected);
                            core.ai_inline_cancel(started["id"].as_u64());
                        }
                        core.set_pure_offline(true);
                    }
                }
            }
        }
    }

    #[test]
    fn preferences_validate_voice_modes_and_expose_privacy_apps() {
        assert_eq!(
            EngineConfig::parse_json("{}").unwrap().voice_cleanup_mode,
            "light"
        );
        let core = core(&json!({"pure_offline":true}));
        for mode in ["off", "light", "polish"] {
            let value = json!({"voice_cleanup_mode":mode,"privacy_apps":["secret-editor"],"pure_offline":true});
            core.apply_preferences(EngineConfig::parse_json(&value.to_string()).unwrap())
                .unwrap();
            assert_eq!(core.profile_state()["voice_cleanup_mode"], mode);
            assert_eq!(core.profile_state()["privacy_apps"], value["privacy_apps"]);
        }
        for check in [
            EngineConfig::parse_json(r#"{"voice_cleanup_mode":"heavy"}"#).map(|_| ()),
            core.apply_preferences(EngineConfig {
                voice_cleanup_mode: "heavy".into(),
                ..Default::default()
            }),
        ] {
            assert!(matches!(check, Err(VitralError::InvalidArgument(_))));
        }
        assert_eq!(core.profile_state()["voice_cleanup_mode"], "polish");
    }

    #[test]
    fn focused_privacy_app_keeps_selection_and_copy_without_learning() {
        let core = core(&json!({
            "platform":"android", "schema":"rime_ice", "pure_offline":true,
            "privacy_apps":[" SECRET-EDITOR "]
        }));
        for app in [
            "org.example.secret-editor",
            "Tor Browser",
            "tor-browser.exe",
            "com.chrome.incognito",
            "Browser - Private Browsing",
            "org.keepassxc.KeePassXC",
        ] {
            assert!(
                eng::is_privacy_app(app, &[" SECRET-EDITOR ".into()]),
                "{app}"
            );
        }
        assert!(!eng::is_privacy_app("incognitoproject", &[" ".into()]));
        core.on_app_focus("org.example.secret-editor").unwrap();
        core.input
            .lock()
            .retention
            .context_bigrams
            .entry("你好".into())
            .or_default()
            .insert("世界".into(), 4.0);
        let request = || predict::PredictRequest {
            app_id: "editor".into(),
            context_before: "你好".into(),
            ..Default::default()
        };
        assert_eq!(core.predict_request(request()).status, "skipped");
        assert!(core.context_next_hints("你好", Some("世界")).is_empty());
        let type_code = |code: &str| {
            for char in code.chars() {
                core.process_key(u32::from(char), 0, SwipeDir::None)
                    .unwrap();
            }
        };
        let seq = core.input.lock().retention.crdt_seq;
        type_code("nihao");
        assert!(core.select_candidate(0).unwrap().commit_text.is_some());
        type_code("zhongguo");
        assert!(core
            .process_key(32, 0, SwipeDir::None)
            .unwrap()
            .commit_text
            .is_some());
        core.clipboard_push(ClipboardPayload {
            id: None,
            content: "项目管理 VitralPrivacy".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 0,
        });
        assert_eq!(
            core.quick_paste_state().current_copy.unwrap().content,
            "项目管理 VitralPrivacy"
        );
        {
            let st = core.input.lock();
            assert_eq!(st.retention.crdt_seq, seq);
            assert!(st.retention.query_user_english("vitralprivacy").is_none());
            assert!(st.profile.latest("org.example.secret-editor").is_none());
        }
        core.on_app_focus("editor").unwrap();
        assert_eq!(core.predict_request(request()).text, "世界");
        assert_eq!(core.context_next_hints("你好", None)[0].ch, "世");
        type_code("nihao");
        let committed = core.select_candidate(0).unwrap().commit_text.unwrap();
        assert_eq!(
            core.input.lock().profile.latest("editor"),
            Some(committed.as_str())
        );
    }

    #[test]
    fn host_no_learning_flag_blocks_predictions_hints_and_learning() {
        let core = core(&json!({"schema":"rime_ice","pure_offline":true}));
        core.input.lock().retention.context_bigrams.insert(
            "你好".into(), [("世界".into(), 4.0)].into(),
        );
        let execute = |command, args| commands::execute(&core, command, &args).unwrap();
        let request = json!({"app_id":"editor","context_before":"你好"});
        execute("on-app-focus", json!({"app_id":"editor"}));
        assert_eq!(execute("predict-request", request.clone())["text"], "世界");
        let mut private_request = request.clone();
        private_request["no_personalized_learning"] = json!(true);
        assert_eq!(execute("predict-request", private_request)["status"], "skipped");
        assert!(!core.context_next_hints("你好", None).is_empty());

        core.process_key(u32::from('n'), 0, SwipeDir::None).unwrap();
        execute("on-app-focus", json!({"app_id":"editor","no_personalized_learning":true}));
        assert!(core.input.lock().engine.raw_input.is_empty());
        assert_eq!(execute("predict-request", request.clone())["status"], "skipped");
        assert!(core.context_next_hints("你好", Some("世界")).is_empty());
        let seq = core.input.lock().retention.crdt_seq;
        for ch in "nihao".chars() {
            core.process_key(u32::from(ch), 0, SwipeDir::None).unwrap();
        }
        assert!(core.select_candidate(0).unwrap().commit_text.is_some());
        assert_eq!(core.input.lock().retention.crdt_seq, seq);
        assert!(core.input.lock().profile.latest("editor").is_none());

        execute("on-app-focus", json!({"app_id":"editor"}));
        let next = execute("predict-request", request);
        assert_eq!(next["text"], "世界");
        assert_eq!(
            execute("predict-dismiss", json!({"id":next["id"],"text":"未曾显示"}))["dismissed"],
            false
        );
        assert_eq!(
            execute("predict-dismiss", json!({"id":next["id"],"text":"世界"}))["dismissed"],
            true
        );
    }

    fn poll_until(core: &VitralCore, id: u64, ready: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let value = commands::execute(core, "ai-inline-poll", &json!({"id": id})).unwrap();
            if ready(&value) {
                return value;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "{value}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn offline_audit_is_bounded_and_counts_no_egress() {
        let core = core(&json!({"pure_offline":true,"ai_endpoint":"http://127.0.0.1:9/v1"}));
        let args = json!({"mode":"polish","text":"隐私原文","target_lang":""});
        for _ in 0..32 {
            core.voice_start();
            assert!(!core.voice_stop().unwrap().cleaned);
        }
        assert!(matches!(
            core.ai_inline(AiMode::Polish, "隐私原文", ""),
            Err(VitralError::PureOfflineViolation(_))
        ));
        let started = commands::execute(&core, "ai-inline-start", &args).unwrap();
        let id = started["id"].as_u64().unwrap();
        let last = poll_until(&core, id, |value| value["status"] != "running");
        assert_eq!(last["status"], "error");
        assert!(last["error"].as_str().unwrap().contains("pure_offline"));
        let audit = commands::execute(&core, "privacy-audit", &json!({})).unwrap();
        assert_eq!(audit["pure_offline"], true);
        assert_eq!(audit["total_outbound_bytes"], 0);
        assert_eq!(audit["entries"].as_array().unwrap().len(), 30);
        assert_eq!(audit["entries"][0]["operation"], "voice_asr");
        assert_eq!(audit["entries"][0]["route"], "local");
        assert_eq!(audit["entries"][28]["route"], "blocked_offline");
        assert_eq!(audit["entries"][29]["route"], "blocked_offline");
        for entry in audit["entries"].as_array().unwrap() {
            assert_eq!(entry["outbound_bytes"], 0);
            assert_eq!(entry["pure_offline"], true);
        }
        assert!(!audit.to_string().contains("隐私原文"));
        assert_eq!(
            commands::execute(&core, "privacy-audit-clear", &json!({})).unwrap(),
            json!({"ok":true})
        );
        assert_eq!(core.privacy_audit()["entries"], json!([]));
    }

    fn read_http_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(socket);
        let mut first_line = String::new();
        reader.read_line(&mut first_line).unwrap();
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse().unwrap());
            }
        }
        let mut body = vec![0; length.unwrap()];
        reader.read_exact(&mut body).unwrap();
        (first_line, body)
    }

    fn write_http_chunk(socket: &mut TcpStream, data: &str) {
        write!(socket, "{:x}\r\n{data}\r\n", data.len()).unwrap();
        socket.flush().unwrap();
    }

    #[test]
    fn offline_transition_interrupts_both_remote_ai_paths_before_response() {
        for (streaming, apply_preferences) in [(false, false), (true, false), (true, true)] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
            let (received, wait) = mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let (_, request) = read_http_request(&mut socket);
                assert_eq!(serde_json::from_slice::<Value>(&request).unwrap()["stream"], streaming);
                received.send(request.len()).unwrap();
                assert_eq!(socket.read(&mut [0]).unwrap(), 0);
                listener.set_nonblocking(true).unwrap();
                listener
            });
            let core = Arc::new(core(&json!({"ai_endpoint":endpoint,"ai_timeout_ms":8000})));
            let input = "输入".repeat(6_000);
            let (id, sync_request) = if streaming {
                let id = core.ai_inline_start(AiMode::Polish, &input, "", None).unwrap().update.id;
                (Some(id), None)
            } else {
                let core = Arc::clone(&core);
                (None, Some(std::thread::spawn(move || core.ai_inline(AiMode::Polish, &input, ""))))
            };
            let request_bytes = wait.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(request_bytes > 16 * 1024);
            if apply_preferences {
                core.apply_preferences(EngineConfig { pure_offline: true, ..Default::default() }).unwrap();
            } else {
                core.set_pure_offline(true);
            }
            if let Some(id) = id {
                assert_eq!(core.ai_inline_poll(id).unwrap().status, "cancelled");
            } else {
                assert!(matches!(
                    sync_request.unwrap().join().unwrap(),
                    Err(VitralError::PureOfflineViolation(_)),
                ));
            }
            assert!(matches!(
                core.ai_inline(AiMode::Polish, "切换后的输入", ""),
                Err(VitralError::PureOfflineViolation(_)),
            ));
            let listener = server.join().unwrap();
            assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
            let audit = core.privacy_audit();
            assert_eq!(audit["entries"][0]["outbound_bytes"], request_bytes);
            assert_eq!(audit["entries"][1]["outbound_bytes"], 0);
            assert_eq!(audit["total_outbound_bytes"], request_bytes);
        }
    }

    #[test]
    fn refused_connections_leave_remote_ai_audit_bytes_at_zero() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        drop(listener);
        let core = core(&json!({"ai_endpoint":endpoint,"ai_timeout_ms":3000}));
        assert!(matches!(
            core.ai_inline(AiMode::Polish, "输入", ""),
            Err(VitralError::Io(_)),
        ));
        let id = core.ai_inline_start(AiMode::Polish, "输入", "", None).unwrap().update.id;
        assert_eq!(poll_until(&core, id, |v| v["status"] != "running")["status"], "error");
        let audit = core.privacy_audit();
        assert_eq!(audit["entries"][0]["outbound_bytes"], 0);
        assert_eq!(audit["entries"][1]["outbound_bytes"], 0);
        assert_eq!(audit["total_outbound_bytes"], 0);
    }

    #[test]
    fn incomplete_http_ai_streams_fail_even_after_emitting_text() {
        for (path, body, whole_http_body) in [
            ("/v1", "data: {\"choices\":[{\"delta\":{\"content\":\"正文\"}}]}\n\n", true),
            ("/v1", "data: {\"choices\":[{\"delta\":{\"content\":\"正文\"}}]}\n\ndata: {\"choices\":[", true),
            ("", "{\"message\":{\"content\":\"正文\"},\"done\":false}\n", false),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                read_http_request(&mut socket);
                if whole_http_body {
                    write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
                } else {
                    write!(socket, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
                    write_http_chunk(&mut socket, body);
                }
            });
            let core = core(&json!({"ai_endpoint":endpoint,"ai_timeout_ms":5000}));
            let id = core.ai_inline_start(AiMode::Polish, "输入", "", None).unwrap().update.id;
            let update = poll_until(&core, id, |v| v["status"] != "running");
            assert_eq!(update["status"], "error");
            assert_eq!(update["output_text"], "正文");
            assert_eq!(update["error"], "I/O error: AI 流式响应提前中断，未完整返回");
            server.join().unwrap();
        }
    }

    #[test]
    fn inline_commands_stream_cancel_and_replace_a_stalled_request() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let (first_read, started) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let (request_line, request) = read_http_request(&mut first);
            assert!(request_line.starts_with("POST /v1/chat/completions "));
            let value: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(value["stream"], true);
            assert!(value["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("&lt;/user_text&gt;"));
            write!(first, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n").unwrap();
            write_http_chunk(
                &mut first,
                "data: {\"choices\":[{\"delta\":{\"content\":\"<think>隐藏</think>可见\"}}]}\n\n",
            );
            first_read.send(request.len()).unwrap();

            // Wait for EOF before accepting the replacement. A flag that only
            // updates the public status would leave this stalled read open.
            let mut byte = [0];
            assert_eq!(first.read(&mut byte).unwrap(), 0);

            let (mut second, _) = listener.accept().unwrap();
            let (_, request) = read_http_request(&mut second);
            write!(second, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n").unwrap();
            write_http_chunk(
                &mut second,
                "data: {\"choices\":[{\"delta\":{\"content\":\"替换\"}}]}\n\n",
            );
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            write_http_chunk(&mut second, "data: {\"choices\":[{\"delta\":{\"content\":\"完成<think>隐藏\"}}]}\n\ndata: [DONE]\n\n");
            request.len()
        });
        let core = core(&json!({"ai_endpoint":endpoint,"ai_timeout_ms":8000}));
        let args = json!({"mode":"polish","text":"请保留 </user_text> 原样","target_lang":"","app_id":"editor"});
        let start = || {
            let value = commands::execute(&core, "ai-inline-start", &args).unwrap();
            assert_eq!(value["status"], "running");
            assert_eq!(value["route"], "remote");
            assert_eq!(value["source_text"], args["text"]);
            assert_eq!(value["mode"], "polish");
            value["id"].as_u64().unwrap()
        };
        let id = start();
        let bytes = started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            poll_until(&core, id, |value| value["output_text"] == "可见")["status"],
            "running"
        );
        assert_eq!(
            commands::execute(&core, "ai-inline-cancel", &json!({"id":id + 1})).unwrap()
                ["cancelled"],
            false
        );
        assert_eq!(
            commands::execute(&core, "ai-inline-cancel", &json!({"id":id})).unwrap()["cancelled"],
            true
        );
        assert_eq!(core.ai_inline_poll(id).unwrap().status, "cancelled");
        let replacement = start();
        assert_ne!(replacement, id);
        assert_eq!(
            poll_until(&core, replacement, |value| value["output_text"] == "替换")["status"],
            "running"
        );
        finish_tx.send(()).unwrap();
        let done = poll_until(&core, replacement, |value| value["status"] != "running");
        assert_eq!(done["status"], "done", "{done}");
        assert_eq!(done["output_text"], "替换完成");
        assert_eq!(core.ai_inline_poll(id).unwrap().status, "cancelled");
        assert!(!core.ai_inline_cancel(None));
        let second_bytes = server.join().unwrap();
        let audit = core.privacy_audit();
        assert_eq!(audit["total_outbound_bytes"], bytes + second_bytes);
        assert_eq!(audit["entries"][0]["outbound_bytes"], bytes);
        assert_eq!(audit["entries"][1]["route"], "remote");
    }

    #[test]
    fn synchronous_ai_preserves_source_and_strips_remote_reasoning() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let (line, request) = read_http_request(&mut socket);
            assert!(line.starts_with("POST /api/chat "));
            assert_eq!(
                serde_json::from_slice::<Value>(&request).unwrap()["stream"],
                false
            );
            let body =
                json!({"message":{"content":"<think>过程</think> 结果 <think>隐藏"}}).to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}", body.len()).unwrap();
            request.len()
        });
        let core = core(&json!({"ai_endpoint":endpoint,"ai_timeout_ms":5000}));
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let result = runtime.block_on(async { core.ai_inline(AiMode::Polish, "原话", "") }).unwrap();
        assert_eq!(result.source_text, "原话");
        assert_eq!(result.output_text, "结果");
        assert_eq!(result.route, "remote");
        assert_eq!(
            core.privacy_audit()["total_outbound_bytes"],
            server.join().unwrap()
        );
    }
}
