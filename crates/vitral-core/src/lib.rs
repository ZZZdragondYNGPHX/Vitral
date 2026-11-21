#[cfg(windows)]
pub mod windows_host;
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
pub mod offline_guard;
#[cfg(any(feature = "native-voice", feature = "native-onnx"))]
mod onnx_env;
pub mod ret;
pub mod sync;

use crate::ai::{AiClient, AiInlineResult, AiMode, VoiceResult, VoiceSession};
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
use crate::error::VitralError;
use crate::hw::{
    HandwritingPayload, HandwritingSession, HwDecodeContext, HwMode, Mode2Result, StrokePoint,
};
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
use std::sync::Arc;
use vitral_tokens::validate_generated_parity;

pub struct InputState {
    pub engine: EngineState,
    pub hw: HandwritingSession,
    pub retention: RetentionEngine,
    pub voice: VoiceSession,
}

pub struct SyncState {
    pub clipboard: ClipboardManager,
    pub devices: Vec<RelayDevice>,
    pub dav_config: Option<DavConfig>,
    pub device_id: String,
    pub device_name: String,
    cloud_ring_seen: bool,
    backup_pending: bool,
    backup_generation: u64,
    last_cleanup: Option<std::time::Instant>,
    clipboard_changes: clip::RingChanges,
}

pub struct VitralCore {
    local_dir: Option<std::path::PathBuf>,
    clipboard_io: Mutex<()>,
    lan_session: Mutex<Option<lan::DiscoverySession>>,
    ai: RwLock<Arc<AiClient>>,
    offline_guard: OfflineGuard,
    imported_lexicon: Arc<RwLock<ImportedLexiconIndex>>,
    input: Mutex<InputState>,
    sync: Mutex<SyncState>,
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

        let config: EngineConfig = if config_json.trim().is_empty() {
            EngineConfig::default()
        } else {
            EngineConfig::parse_json(config_json)?
        };
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
        let mut devices = config.known_devices.clone();
        sort_devices_mru(&mut devices);

        let ai = AiClient::from(&config);
        let mut engine = EngineState::new(config);
        let imported_lexicon = Arc::new(RwLock::new(ImportedLexiconIndex::new(
            engine.resolved_assets_dir.as_deref(),
        )));
        let hw = HandwritingSession::with_assets_dir(engine.resolved_assets_dir.as_deref());

        let local_dir = (!engine.config.data_dir.is_empty()).then(|| std::path::PathBuf::from(&engine.config.data_dir));
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
            if let Some(snapshot) = local::load::<LexiconSnapshotPayload>(dir,"lexicon-state.json")? {
                retention=snapshot.retention;
                imported_lexicon.write().inject_hotwords(&snapshot.imported_entries);
            }
            if let Some(memory)=local::load(dir,"app-languages.json")? {engine.app_lang_memory=memory;}
            if let Some(ring)=local::load(dir,"clipboard-history.json")? { clipboard.restore_ring_cold_boot(ring); }
        }
        let backup_pending = match &local_dir {
            Some(dir) => local::load(dir, "backup-pending.json")?.unwrap_or(false),
            None => false,
        };
        let clipboard_changes = match &local_dir {
            Some(dir) => local::load(dir, "clipboard-changes.json")?.unwrap_or_default(),
            None => Default::default(),
        };
        Ok(Self {
            local_dir,
            clipboard_io:Mutex::new(()),
            lan_session: Mutex::new(None),
            ai: RwLock::new(Arc::new(ai)),
            offline_guard: OfflineGuard::new(pure_offline),
            imported_lexicon,
            input: Mutex::new(InputState {
                engine,
                hw,
                retention,
                voice: VoiceSession::default(),
            }),
            sync: Mutex::new(SyncState {
                clipboard,
                devices,
                cloud_ring_seen: false,
                backup_pending,
                backup_generation: 0,
                last_cleanup: None,
                clipboard_changes,
                dav_config,
                device_id,
                device_name,
            }),
        })
    }

    pub fn imported_lexicon(&self) -> Arc<RwLock<ImportedLexiconIndex>> {
        Arc::clone(&self.imported_lexicon)
    }

    fn save_local(&self, name:&str, value:&impl serde::Serialize) {
        if let Some(dir)=&self.local_dir {
            if let Err(e)=local::save(dir,name,value){eprintln!("Vitral: failed to save {name}: {e}");}
        }
    }

    fn sync_tier1_best_effort(&self) {
        let _ = self.sync_tier1(false);
    }

    fn sync_tier1(&self, flush: bool) -> Result<(), VitralError> {
        let (dav_cfg, profile, snapshot, crdt_delta, generation) = {
            let st = self.input.lock();
            let mut sync_st = self.sync.lock();
            if self.local_dir.is_none() && sync_st.dav_config.is_none() { return Ok(()); }
            if flush && !sync_st.backup_pending { return Ok(()); }
            let imported = self.imported_lexicon.read();
            let profile = ProfileBackup {
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
                imported_entries: imported.export_entries(),
            };
            if !flush {
                self.save_local("lexicon-state.json", &snapshot);
                self.save_local("app-languages.json", &profile.app_lang_memory);
                if st.engine.config.background_sync && sync_st.dav_config.is_some() {
                    sync_st.backup_pending = true;
                    sync_st.backup_generation = sync_st.backup_generation.wrapping_add(1);
                    self.save_local("backup-pending.json", &true);
                    return Ok(());
                }
            }
            if self.is_pure_offline() { return Ok(()); }
            let Some(ref cfg) = sync_st.dav_config else { return Ok(()) };
            let crdt_delta = st.retention.export_crdt_delta(&sync_st.device_id);
            (cfg.clone(), profile, snapshot, crdt_delta, sync_st.backup_generation)
        };
        upload_profile_config(&self.offline_guard, &dav_cfg, &profile)?;
        upload_lexicon_snapshot(&self.offline_guard, &dav_cfg, &snapshot)?;
        upload_retention_crdt_delta(&self.offline_guard, &dav_cfg, &crdt_delta)?;
        if flush {
            let mut s = self.sync.lock();
            if s.backup_generation == generation && s.dav_config.as_ref() == Some(&dav_cfg) {
                s.backup_pending = false;
                self.save_local("backup-pending.json", &false);
            }
        }
        Ok(())
    }

    /// Called on the platform service queue, away from the keystroke path.
    pub fn poll_services(&self) -> Result<QuickPasteState, VitralError> {
        let clipboard = self.poll_clipboard()?;
        if self.is_pure_offline() { return Ok(clipboard); }
        self.sync_tier1(true)?;
        let cleanup = {
            let mut s = self.sync.lock();
            let due = s.dav_config.is_some() && s.last_cleanup.is_none_or(|last| last.elapsed().as_secs() >= 3600);
            if due { s.last_cleanup = Some(std::time::Instant::now()); }
            due
        };
        if cleanup { self.relay_cleanup_orphans()?; }
        Ok(clipboard)
    }

    pub fn poll_clipboard(&self) -> Result<QuickPasteState, VitralError> {
        let _io = self.clipboard_io.lock();
        let (cfg, before, changes, capacity) = {
            let s = self.sync.lock();
            let Some(cfg) = s.dav_config.as_ref().filter(|_| !self.is_pure_offline()) else {
                return Ok(s.clipboard.quick_paste_state());
            };
            (cfg.clone(), s.clipboard.export_ring(), s.clipboard_changes.clone(), s.clipboard.capacity())
        };
        let fetched = if changes.is_empty() {
            dav::download_clipboard_ring(&self.offline_guard, &cfg)?
        } else {
            Some(dav::merge_clipboard_ring(&self.offline_guard, &cfg, &changes, &before, capacity)?)
        };
        let mut s = self.sync.lock();
        if !self.is_pure_offline() && s.dav_config.as_ref() == Some(&cfg)
            && s.clipboard.export_ring() == before && s.clipboard_changes == changes {
            let first = !s.cloud_ring_seen;
            s.cloud_ring_seen = true;
            if !changes.is_empty() {
                s.clipboard_changes = Default::default();
                self.save_local("clipboard-changes.json", &s.clipboard_changes);
            }
            if let Some(entries) = fetched.filter(|entries| entries != &before) {
                s.clipboard.observe_cloud_ring(entries, first);
                self.save_local("clipboard-history.json", &s.clipboard.export_ring());
            }
        }
        Ok(s.clipboard.quick_paste_state())
    }

    fn record_clipboard_change(&self, s: &mut SyncState, before: &[clip::ClipboardEntry]) {
        let after = s.clipboard.export_ring();
        if s.dav_config.is_some() {
            s.clipboard_changes.record(before, &after);
            self.save_local("clipboard-changes.json", &s.clipboard_changes);
        }
        self.save_local("clipboard-history.json", &after);
    }

    /// Apply settings without replacing composing, clipboard or retention state.
    pub fn apply_preferences(&self, cfg: EngineConfig) -> Result<(), VitralError> {
        if !(20..=200).contains(&cfg.clipboard_ring_capacity) {
            return Err(VitralError::InvalidArgument("clipboard capacity must be 20..200".into()));
        }
        let mut st = self.input.lock();
        let mut sync = self.sync.lock();
        self.set_pure_offline(cfg.pure_offline);
        if st.engine.schema != cfg.schema { st.engine.set_schema(cfg.schema); }
        st.engine.set_opencc_mode(cfg.opencc_mode);
        if let Some(seconds) = cfg.frecency_half_life_sec { st.retention.set_half_life_sec(seconds); }
        sync.clipboard.set_capacity(cfg.clipboard_ring_capacity);
        let next = DavConfig::from_engine_config(&cfg);
        if sync.dav_config != next {
            sync.cloud_ring_seen = false;
            sync.last_cleanup = None;
            sync.backup_pending = false;
            self.save_local("backup-pending.json", &false);
            sync.clipboard_changes = Default::default();
            self.save_local("clipboard-changes.json", &sync.clipboard_changes);
            sync.dav_config = next;
        }
        *self.ai.write() = Arc::new(AiClient::from(&cfg));
        let active = &mut st.engine.config;
        active.schema = cfg.schema;
        active.opencc_mode = cfg.opencc_mode;
        active.clipboard_ring_capacity = cfg.clipboard_ring_capacity;
        active.koofr_endpoint = cfg.koofr_endpoint;
        active.koofr_username = cfg.koofr_username;
        active.koofr_password = cfg.koofr_password;
        active.koofr_root = cfg.koofr_root;
        active.sync_key = cfg.sync_key;
        active.ai_tier3_enabled = cfg.ai_tier3_enabled;
        Ok(())
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
        self.offline_guard.set_pure_offline(enabled);
        if enabled { self.lan_session.lock().take(); }
    }

    pub fn is_pure_offline(&self) -> bool {
        self.offline_guard.is_pure_offline()
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
        let rep = {
            let mut st = self.input.lock();
            let rep = st.engine.reload_assets()?;
            let dir = st.engine.resolved_assets_dir.clone();
            st.hw.set_assets_dir(dir.as_deref());
            rep
        };
        self.sync_tier1_best_effort();
        Ok(rep)
    }

    pub fn on_app_focus(&self, app_id: &str) -> Result<AppFocusState, VitralError> {
        let mut st = self.input.lock();
        st.engine.on_app_focus(app_id)
    }

    pub fn process_key(
        &self,
        keycode: u32,
        modifiers: u32,
        swipe_dir: SwipeDir,
    ) -> Result<InputAction, VitralError> {
        let (act, seq_changed) = {
            let mut st = self.input.lock();
            let before_seq = st.retention.crdt_seq;
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            let sync_st = self.sync.lock();
            let imported = self.imported_lexicon.read();
            let act = engine.process_key(
                keycode,
                modifiers,
                swipe_dir,
                &sync_st.clipboard,
                &imported,
                retention,
            )?;
            if modifiers & eng::MOD_LANG_TOGGLE != 0 { self.save_local("app-languages.json",&engine.app_lang_memory); }
            (act, retention.crdt_seq != before_seq || modifiers & eng::MOD_LANG_TOGGLE != 0)
        };
        if seq_changed {
            self.sync_tier1_best_effort();
        }
        Ok(act)
    }

    pub fn select_candidate(&self, index: usize) -> Result<InputAction, VitralError> {
        let (act, seq_changed) = {
            let mut st = self.input.lock();
            let before_seq = st.retention.crdt_seq;
            let canonical = st.engine.canonical_at(index).map(str::to_string);
            let sync_st = self.sync.lock();
            let imported = self.imported_lexicon.read();
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            let committed = engine.select_candidate_on_backend(
                index,
                &sync_st.clipboard,
                &imported,
                retention,
            )?;
            if let Some(ref rec_key) = canonical {
                retention.record_selection(rec_key, 0.0, 0.001, 1.0);
            } else if let Some(ref commit_text) = committed {
                retention.record_selection(commit_text, 0.0, 0.001, 1.0);
            }
            (
                engine.empty_action(committed),
                retention.crdt_seq != before_seq,
            )
        };
        if seq_changed {
            self.sync_tier1_best_effort();
        }
        Ok(act)
    }

    pub fn pin_candidate(&self, index: usize) -> Result<InputAction, VitralError> {
        let act = {
            let mut st = self.input.lock();
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            engine.pin_candidate_in_state(index, retention)?;
            engine.empty_action(None)
        };
        self.sync_tier1_best_effort();
        Ok(act)
    }

    pub fn erase_candidate(&self, index: usize) -> Result<InputAction, VitralError> {
        let act = {
            let mut st = self.input.lock();
            let InputState {
                ref mut engine,
                ref mut retention,
                ..
            } = *st;
            engine.erase_candidate_in_state(index, retention)?;
            engine.empty_action(None)
        };
        self.sync_tier1_best_effort();
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
        let gram = st.engine.resolved_assets_dir.as_ref().and_then(|d| {
            crate::eng::gram::RimeGramDb::open_cached(
                &d.join("rime").join("wanxiang-lts-zh-hans.gram"),
            )
            .ok()
        });
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
        let (payload, revision, refine) = {
            let mut st = self.input.lock();
            let gram = st.engine.resolved_assets_dir.as_ref().and_then(|d| {
                crate::eng::gram::RimeGramDb::open_cached(&d.join("rime/wanxiang-lts-zh-hans.gram")).ok()
            });
            let imported = self.imported_lexicon.read();
            let refine = st.engine.config.ai_tier3_enabled && !self.is_pure_offline();
            let InputState { hw, retention, .. } = &mut *st;
            let ctx = HwDecodeContext {imported:&imported,gram:gram.as_deref(),retention,tier3:None,offline_guard:Some(&self.offline_guard)};
            let payload = hw.end_with_context(&ctx)?;
            let revision = hw.revision;
            let refine = refine && !hw.confirmed_since_edit;
            if let HandwritingPayload::Mode1(result) = &payload {
                st.engine.lookup_hints.clone_from(&result.candidates);
            }
            (payload, revision, refine)
        };
        let HandwritingPayload::Mode2(local) = &payload else { return Ok(payload) };
        let mut choices = Vec::new();
        if refine {
            let ai = Arc::clone(&self.ai.read());
            for slot in local.slots.iter().filter(|s| !s.locked_by_entropy && !s.manually_locked && s.top8.len() > 1) {
                let options:Vec<_> = slot.top8.iter().map(|c| c.ch.as_str()).collect();
                let prompt = "Choose one character from these handwriting candidates. Return JSON with slot_index equal to its zero-based array index. Preserve the local choice if uncertain.";
                let question = serde_json::json!({"candidates":options,"local_choice":slot.ch});
                let choice = ai.chat(&self.offline_guard, prompt, &question.to_string()).ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                    .and_then(|v| v["slot_index"].as_u64())
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| options.get(i).copied());
                if let Some(ch) = choice {choices.push((slot.slot_idx,ch.to_owned()));}
            }
        }
        let (payload, confirmed) = {
            let mut st = self.input.lock();
            let InputState { hw, retention, .. } = &mut *st;
            if hw.revision != revision { return Err(VitralError::InvalidArgument("handwriting changed during recognition".into())); }
            if !hw.confirmed_since_edit && !self.is_pure_offline() {
                for (i,ch) in choices {
                    let slot=&mut hw.last_slots[i];
                    slot.changed_retroactively |= slot.ch != ch;
                    slot.ch=ch;
                }
            }
            (hw.snapshot()?,hw.confirm_result(retention).is_some())
        };
        if confirmed {
            self.sync_tier1_best_effort();
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

    pub fn hw_confirm(&self) -> Result<String, VitralError> {
        let mut st = self.input.lock();
        let InputState {
            ref mut hw,
            ref mut retention,
            ..
        } = *st;
        hw.confirm_result(retention)
            .ok_or_else(|| VitralError::NotFound("no handwriting result to confirm".into()))
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
            self.sync_tier1_best_effort();
        }
        Ok(rep)
    }

    pub fn clipboard_push(&self, payload: ClipboardPayload) -> ClipboardState {
        let state = self.clipboard_push_local(payload);
        let _ = self.poll_clipboard();
        state
    }

    fn clipboard_push_local(&self, mut payload: ClipboardPayload) -> ClipboardState {
        let mut sync = self.sync.lock();
        if payload.source_device.is_empty() {
            payload.source_device.clone_from(&sync.device_id);
        }
        if payload.timestamp_ms == 0 {
            payload.timestamp_ms = RetentionEngine::now_epoch_ms();
        }
        let before = sync.clipboard.export_ring();
        let state = sync.clipboard.push(payload);
        self.record_clipboard_change(&mut sync, &before);
        state
    }

    pub fn clipboard_ingest_remote(
        &self,
        payload: ClipboardPayload,
        is_cold_boot: bool,
    ) -> QuickPasteState {
        let mut sync_st = self.sync.lock();
        let state=sync_st.clipboard.ingest_remote(payload, is_cold_boot);
        self.save_local("clipboard-history.json",&sync_st.clipboard.export_ring());
        state
    }

    pub fn clipboard_state(&self) -> ClipboardState {
        let sync_st = self.sync.lock();
        sync_st.clipboard.state()
    }

    pub fn quick_paste_state(&self) -> QuickPasteState {
        let sync_st = self.sync.lock();
        sync_st.clipboard.quick_paste_state()
    }

    pub fn consume_quick_paste(&self) -> QuickPasteConsumeResult {
        let mut sync_st = self.sync.lock();
        sync_st.clipboard.consume_quick_paste()
    }

    pub fn on_ime_open(&self) -> QuickPasteState {
        let mut sync_st = self.sync.lock();
        sync_st.clipboard.on_ime_open()
    }

    pub fn on_ime_close(&self) -> QuickPasteState {
        let mut sync_st = self.sync.lock();
        sync_st.clipboard.on_ime_close()
    }

    pub fn dismiss_quick_paste(&self) -> QuickPasteState {
        let mut sync_st = self.sync.lock();
        sync_st.clipboard.dismiss_quick_paste()
    }

    pub fn clipboard_pin(
        &self,
        item_id: &str,
        shortcut_code: Option<&str>,
    ) -> Result<ClipboardState, VitralError> {
        let state = {
            let mut sync_st = self.sync.lock();
            let before = sync_st.clipboard.export_ring();
            let state = sync_st.clipboard.pin(item_id, shortcut_code)?;
            self.record_clipboard_change(&mut sync_st, &before);
            state
        };
        let _ = self.poll_clipboard();
        Ok(state)
    }

    pub fn clipboard_clear_unpinned(&self) -> ClipboardState {
        let state = {
            let mut sync_st = self.sync.lock();
            let before = sync_st.clipboard.export_ring();
            let state = sync_st.clipboard.clear_unpinned();
            self.record_clipboard_change(&mut sync_st, &before);
            state
        };
        let _ = self.poll_clipboard();
        state
    }

    pub fn discover_devices(&self) -> Result<Vec<RelayDevice>, VitralError> {
        let (id, name) = {
            let s = self.sync.lock();
            (s.device_id.clone(), s.device_name.clone())
        };
        {
            let mut session = self.lan_session.lock();
            self.offline_guard.require_online("device discovery")?;
            if session.is_none() {
                *session = Some(lan::DiscoverySession::start(&self.offline_guard, &id, &name)?);
            }
        }
        let peers = lan::LocalSendTransport::discover_mdns_peers(&self.offline_guard,
            std::time::Duration::from_millis(1500))?;
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
        self.save_local("relay-devices.json", &s.devices);
        Ok(s.devices.clone())
    }

    pub fn stop_discovery(&self) {
        self.lan_session.lock().take();
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
        let res = send_via_relay_with_config(
            &self.offline_guard,
            &mut devices,
            device_id,
            payload_or_path,
            is_file_path,
            dav_cfg.as_ref(),
            &local_id,
            &local_name,
        )?;
        {
            let mut sync_st = self.sync.lock();
            if let Some(peer) = sync_st.devices.iter_mut().find(|p| p.device_id == device_id) {
                peer.usage_count = peer.usage_count.saturating_add(1);
                peer.last_used_ms = RetentionEngine::now_epoch_ms().max(peer.last_used_ms.saturating_add(1));
            }
            sort_devices_mru(&mut sync_st.devices);
            self.save_local("relay-devices.json", &sync_st.devices);
        }
        Ok(res)
    }

    pub fn relay_poll_inbox(&self) -> Result<RelayPollResult, VitralError> {
        let (dav_cfg, local_id) = {
            let sync_st = self.sync.lock();
            (sync_st.dav_config.clone(), sync_st.device_id.clone())
        };
        let mut result = poll_koofr_relay_with_config(&self.offline_guard, dav_cfg.as_ref(), &local_id)?;
        if let Some(session) = self.lan_session.lock().as_ref() {
            for item in session.take_received() {
                let id = format!("{}-{}", item.session_id, item.file_id);
                result.received.push(id.clone());
                result.received_items.push(sync::RelayReceivedItem { transfer_id: id,
                    raw_bytes: item.data, is_file: true, file_name: Some(item.file_name) });
            }
        }
        Ok(result)
    }

    pub fn relay_cleanup_orphans(&self) -> Result<CleanupResult, VitralError> {
        let dav_cfg = {
            let sync_st = self.sync.lock();
            sync_st.dav_config.clone()
        };
        cleanup_koofr_orphans_with_config(&self.offline_guard, dav_cfg.as_ref())
    }

    pub fn voice_start(&self) {
        let mut st = self.input.lock();
        st.voice.start();
    }

    pub fn voice_push_pcm(&self, pcm: &[i16]) -> Result<(), VitralError> {
        let mut st = self.input.lock();
        st.voice.push_pcm(pcm)
    }

    pub fn voice_stop(&self) -> Result<VoiceResult, VitralError> {
        let (samples, assets) = {
            let mut st = self.input.lock();
            (st.voice.finish()?, st.engine.resolved_assets_dir.clone())
        };
        ai::recognize_pcm(&samples, assets.as_deref())
    }

    pub fn ai_inline(
        &self,
        mode: AiMode,
        text: &str,
        target_lang: &str,
    ) -> Result<AiInlineResult, VitralError> {
        let ai = Arc::clone(&self.ai.read());
        ai.run(&self.offline_guard, mode, text, target_lang)
    }

    pub fn restore_from_koofr(&self, webdav_cfg_json: &str) -> Result<RestoreResult, VitralError> {
        self.offline_guard.require_online("restore_from_koofr")?;
        let fallback_cfg = {
            let sync_st = self.sync.lock();
            sync_st.dav_config.clone()
        };
        let cfg = DavConfig::from_json_with_fallback(webdav_cfg_json, fallback_cfg.as_ref())?;
        let _clipboard_io = self.clipboard_io.lock();
        let fetched = fetch_backup_from_koofr(&self.offline_guard, &cfg)?;

        {
            let mut st = self.input.lock();
            let mut sync_st = self.sync.lock();
            let mut imported = self.imported_lexicon.write();

            if let Some(profile) = fetched.profile {
                st.engine.set_schema(profile.schema);
                st.engine.set_opencc_mode(profile.opencc_mode);
                st.engine.app_lang_memory = profile.app_lang_memory;
                st.engine.config.clipboard_ring_capacity = profile.clipboard_ring_capacity.max(1);
                st.retention.set_half_life_sec(profile.frecency_half_life_sec);
                sync_st
                    .clipboard
                    .set_capacity(profile.clipboard_ring_capacity);
            }

            if let Some(snap) = fetched.snapshot {
                st.retention.frecency = snap.retention.frecency;
                st.retention.frecency_updated_ms = snap.retention.frecency_updated_ms;
                st.retention.pinned_words = snap.retention.pinned_words;
                st.retention.erased_words = snap.retention.erased_words;
                st.retention.set_half_life_sec(snap.retention.half_life_sec);
                st.retention.crdt_seq = snap.retention.crdt_seq;
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

            if let Some(entries) = fetched.clipboard_ring {
                sync_st.cloud_ring_seen = true;
                sync_st.clipboard_changes = Default::default();
                self.save_local("clipboard-changes.json", &sync_st.clipboard_changes);
                sync_st.clipboard.restore_ring_cold_boot(entries);
            }

            st.engine.config.koofr_endpoint = Some(cfg.endpoint.clone());
            st.engine.config.sync_key = cfg.sync_key.clone();
            sync_st.dav_config = Some(cfg);
            self.save_local("lexicon-state.json",&LexiconSnapshotPayload{schema_version:1,retention:st.retention.clone(),imported_entries:imported.export_entries()});
            self.save_local("app-languages.json",&st.engine.app_lang_memory);
            self.save_local("clipboard-history.json",&sync_st.clipboard.export_ring());
        }

        Ok(RestoreResult {
            restored: true,
            cloud_root: fetched.cloud_root,
            snapshots_applied: fetched.snapshots_applied,
        })
    }
}
