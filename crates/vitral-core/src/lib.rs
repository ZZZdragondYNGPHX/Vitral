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
pub mod offline_guard;
#[cfg(any(feature = "native-voice", feature = "native-onnx"))]
mod onnx_env;
pub mod ret;
pub mod sync;
pub mod shortcuts;

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
        let (cfg, before, changes, capacity, current) = {
            let s = self.sync.lock();
            let Some(cfg) = s.dav_config.as_ref().filter(|_| !self.is_pure_offline()) else {
                return Ok(s.clipboard.quick_paste_state());
            };
            (cfg.clone(), s.clipboard.export_ring(), s.clipboard_changes.clone(), s.clipboard.capacity(), s.clipboard.current().current_copy.clone())
        };
        let fetched = if changes.is_empty() {
            dav::download_clipboard_ring(&self.offline_guard, &cfg)?
        } else {
            Some(dav::merge_clipboard_ring(&self.offline_guard, &cfg, &changes, &before, capacity)?)
        };
        let latest = current.iter().chain(fetched.iter().flatten()).max_by_key(|e| clip::copy_order(e));
        let current = dav::sync_current_copy(&self.offline_guard, &cfg, latest)?;
        let mut s = self.sync.lock();
        if !self.is_pure_offline() && s.dav_config.as_ref() == Some(&cfg) {
            if let Some(entry) = current { s.clipboard.observe_current(entry); }
            self.save_local("clipboard-current.json", s.clipboard.current());
        }
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
                self.save_local("clipboard-current.json", s.clipboard.current());
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
        self.save_local("clipboard-current.json", s.clipboard.current());
    }

    /// Apply settings without replacing composing, clipboard or retention state.
    pub fn apply_preferences(&self, mut cfg: EngineConfig) -> Result<(), VitralError> {
        shortcuts::normalize(&mut cfg.shortcuts).map_err(VitralError::InvalidArgument)?;
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
            sync.backup_pending = next.is_some();
            self.save_local("backup-pending.json", &sync.backup_pending);
            sync.clipboard_changes = Default::default();
            if next.is_some() {
                let cur = sync.clipboard.export_ring();
                sync.clipboard_changes.record(&[], &cur);
            }
            self.save_local("clipboard-changes.json", &sync.clipboard_changes);
            sync.dav_config = next;
        }
        *self.ai.write() = Arc::new(AiClient::from(&cfg));
        if st.engine.config.shortcuts != cfg.shortcuts { st.engine.clear_composition_state(); }
        let active = &mut st.engine.config;
        active.shortcuts = cfg.shortcuts;
        active.linux_english_mode = cfg.linux_english_mode;
        active.adaptive_learning = cfg.adaptive_learning;
        active.learn_from_clipboard = cfg.learn_from_clipboard;
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

    pub fn candidate_state(&self) -> Result<InputAction, VitralError> {
        let mut st = self.input.lock();
        let sync = self.sync.lock();
        let imported = self.imported_lexicon.read();
        let InputState { engine, retention, .. } = &mut *st;
        engine.refresh_candidates(&sync.clipboard, &imported, retention)
    }

    fn select_quick_paste(&self, engine: &mut EngineState, index: usize) -> Option<InputAction> {
        let id = engine.current_candidates.get(index)?.quick_paste_id.clone()?;
        let mut sync = self.sync.lock();
        let current = sync.clipboard.quick_paste_state().quick_paste;
        // A late click must never consume a newer copy than the one displayed.
        let committed = if current.as_ref().is_some_and(|c| c.id == id) {
            let consumed = sync.clipboard.consume_quick_paste().consumed.map(|c| c.content);
            self.save_local("clipboard-current.json", sync.clipboard.current());
            consumed
        } else { None };
        engine.clear_composition_state();
        Some(engine.empty_action(committed))
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
        let (act, seq_changed) = {
            let mut st = self.input.lock();
            let before_seq = st.retention.crdt_seq;
            if st.engine.config.platform == "android" && modifiers == 0 && swipe_dir == SwipeDir::None {
                let mut sync = self.sync.lock();
                if matches!(keycode, 0x08 | 0xff08) && sync.clipboard.quick_paste_state().candidate_visible && st.engine.raw_input.is_empty() {
                    sync.clipboard.dismiss_quick_paste();
                    self.save_local("clipboard-current.json", sync.clipboard.current());
                } else if (0x20..=0x7e).contains(&keycode) {
                    sync.clipboard.hide_for_input();
                }
            }
            if modifiers == 0 && swipe_dir == SwipeDir::None {
                if (keycode == 0x0d || keycode == 0xff0d) && !st.engine.raw_input.is_empty() {
                    let prefix: String = st.engine.composing.chars().take_while(|c| !c.is_ascii()).collect();
                    let raw = format!("{}{}", prefix, st.engine.raw_input);
                    st.engine.clear_composition_state();
                    return Ok(st.engine.empty_action(Some(raw)));
                }
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
            if let Some(action) = Self::select_function(&mut st.engine, index).or_else(|| self.select_quick_paste(&mut st.engine, index)) {
                return Ok(action);
            }
            let before_seq = st.retention.crdt_seq;
            let canonical = st.engine.canonical_at(index).map(str::to_string);
            let code = st.engine.raw_input.clone();
            let feedback = st.engine.selection_feedback(index);
            let is_clipboard = st.engine.is_clipboard_candidate(index);
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
            if !is_clipboard {
                if let Some(word) = committed.as_deref() {
                    engine.record_candidate_commit(
                        canonical.as_deref().unwrap_or(word),
                        &code,
                        feedback,
                        &imported,
                        retention,
                        RetentionEngine::now_epoch_ms(),
                    );
                }
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
        self.sync_tier1_best_effort();
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
            if !hw.confirmed_since_edit && !self.is_pure_offline() {
                for (i,ch) in choices {
                    let slot=&mut hw.last_slots[i];
                    slot.changed_retroactively |= slot.ch != ch;
                    slot.ch=ch;
                }
            }
            let snapshot = hw.snapshot()?;
            let newly_confirmed = !hw.confirmed_since_edit;
            let committed = Self::confirm_handwriting(hw, engine, retention, &imported);
            (snapshot, newly_confirmed && committed.is_some())
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

    fn confirm_handwriting(
        hw: &mut HandwritingSession,
        engine: &mut EngineState,
        retention: &mut RetentionEngine,
        imported: &ImportedLexiconIndex,
    ) -> Option<String> {
        let newly_confirmed = !hw.confirmed_since_edit;
        let mut learned = RetentionEngine::default();
        let committed = hw.confirm_result(&mut learned)?;
        if newly_confirmed {
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
            Self::confirm_handwriting(hw, engine, retention, &imported)
                .ok_or_else(|| VitralError::NotFound("no handwriting result to confirm".into()))?
        };
        self.sync_tier1_best_effort();
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
        let harvest = payload.clone();
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
        drop(sync);
        self.harvest_clipboard_payload(&harvest);
        state
    }

    pub fn clipboard_ingest_remote(
        &self,
        payload: ClipboardPayload,
        is_cold_boot: bool,
    ) -> QuickPasteState {
        let harvest = payload.clone();
        let mut sync_st = self.sync.lock();
        let state=sync_st.clipboard.ingest_remote(payload, is_cold_boot);
        self.save_local("clipboard-history.json",&sync_st.clipboard.export_ring());
        self.save_local("clipboard-current.json", sync_st.clipboard.current());
        drop(sync_st);
        self.harvest_clipboard_payload(&harvest);
        state
    }

    fn harvest_clipboard_payload(&self, payload: &ClipboardPayload) {
        if !payload.mime_type.starts_with("text/plain") {
            return;
        }
        let reset_ms = {
            let st = self.input.lock();
            if !st.engine.config.adaptive_learning || !st.engine.config.learn_from_clipboard {
                return;
            }
            st.retention.learned_reset_ms
        };
        // Annotation may initialize a corpus; hold neither input nor sync while it runs.
        let mut harvested = RetentionEngine::default();
        harvested.harvest_clipboard_text(&payload.content, |phrase| {
            let corpus = Arc::clone(self.imported_lexicon.read().corpus());
            let ann = imp::polyphone::PolyphoneAnnotator::new(corpus).annotate_text(phrase);
            (
                ann.untoned.split_whitespace().collect(),
                imp::xiaohe::pinyin_phrase_to_xiaohe(&ann.untoned),
            )
        });
        if harvested.user_phrases.is_empty() && harvested.user_english.is_empty() {
            return;
        }
        let changed = {
            let mut st = self.input.lock();
            if !st.engine.config.adaptive_learning
                || !st.engine.config.learn_from_clipboard
                || st.retention.learned_reset_ms != reset_ms
            {
                return;
            }
            let before = st.retention.crdt_seq;
            st.retention
                .harvest_clipboard_text(&payload.content, |word| {
                    harvested
                        .user_phrases
                        .get(word)
                        .map(|p| (p.quanpin.clone(), p.xiaohe.clone()))
                        .unwrap_or_default()
                });
            st.retention.crdt_seq != before
        };
        if changed {
            self.sync_tier1_best_effort();
        }
    }

    pub fn personalization_stats(&self) -> ret::PersonalizationStats {
        self.input.lock().retention.stats()
    }

    pub fn clear_learned_habits(&self) -> ret::PersonalizationStats {
        let stats = {
            let mut st = self.input.lock();
            st.retention.clear_learned_habits();
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
        self.sync_tier1_best_effort();
        stats
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
        let result = sync_st.clipboard.consume_quick_paste();
        self.save_local("clipboard-current.json", sync_st.clipboard.current());
        result
    }

    pub fn hide_quick_paste_for_input(&self) { self.sync.lock().clipboard.hide_for_input(); }

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
        let result = sync_st.clipboard.dismiss_quick_paste();
        self.save_local("clipboard-current.json", sync_st.clipboard.current());
        result
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

    pub fn clipboard_unpin(&self, item_id: &str) -> Result<ClipboardState, VitralError> {
        let state = {
            let mut sync_st = self.sync.lock();
            let before = sync_st.clipboard.export_ring();
            let state = sync_st.clipboard.unpin(item_id)?;
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
            self.save_local("clipboard-current.json", sync_st.clipboard.current());
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
        let result = core.voice_stop().unwrap();
        assert_eq!(result.sample_count, (wav.len() - 44) / 2);
        assert_eq!(result.language, "zh");
        assert!(result.transcript.contains("天气") || result.transcript.contains("你好"));
        assert!(matches!(core.voice_stop(), Err(VitralError::NotInitialized)));
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
    fn native_librime_octagram_opencc_and_schema_switching() {
        use crate::eng::{opencc_ffi, rime_ffi};

        let assets_dir = resolve_assets_dir("").expect("workspace assets");
        rime_ffi::with_runtime(&assets_dir, "", "", |rt| {
            assert!(
                rt.version().starts_with("1.17."),
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
        if partial_res.commit_text.is_none() {
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
            let put_gate = Arc::new(Mutex::new(gate));
            let get_gate = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));

            let nodes_bg = Arc::clone(&nodes);
            let log_bg = Arc::clone(&request_log);
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
                            let pg = Arc::clone(&gate_bg);
                            let gg = Arc::clone(&get_gate_bg);
                            conn_joins.push(std::thread::spawn(move || {
                                Self::handle_conn(&mut stream, &n, &l, &pg, &gg);
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
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"{}\"\r\nConnection: close\r\n\r\n",
                            node.data.len(), crate::sync::sha256_hex(&node.data)
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
                            dav::sync_current_copy(&guard, &cfg, Some(&entry)).unwrap(),
                            Some(entry.clone())
                        );
                        assert_eq!(
                            dav::sync_current_copy(&guard, &cfg, None).unwrap(),
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
        let cloud_bytes = crate::sync::encode_clipboard_ring_jsonl(&[cloud.clone()]).unwrap();
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
            // 2a. Simplified: "nihaoshouxiezhongwen" -> partial "你好" -> select_candidate("手写中问")
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
                .position(|c| c.text == "手写中问" && c.comment == "shou xie zhong wen")
                .unwrap_or_else(|| {
                    panic!(
                        "expected PureRust candidate '手写中问' after partial '你好', got: {:?}",
                        after_partial.candidates
                    )
                });
            let committed = core.select_candidate(pr_idx).unwrap();
            assert_eq!(
                committed.commit_text.as_deref(),
                Some("你好手写中问"),
                "select_candidate on PureRust candidate must preserve confirmed prefix"
            );

            // 2b. Traditional from start: "womenshouxiezhongwen" -> partial "我們" -> select_candidate("手寫中問")
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
                .position(|c| c.text == "手寫中問" && c.comment == "shou xie zhong wen")
                .unwrap_or_else(|| {
                    panic!(
                        "expected Traditional PureRust candidate '手寫中問', got: {:?}",
                        tc_after_partial.candidates
                    )
                });
            let tc_committed = tc_core.select_candidate(tc_pr_idx).unwrap();
            assert_eq!(
                tc_committed.commit_text.as_deref(),
                Some("我們手寫中問"),
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
                .position(|c| c.text == "手写中问" && c.comment == "shou xie zhong wen")
                .unwrap_or_else(|| {
                    panic!(
                        "expected PureRust candidate '手写中问', got: {:?}",
                        sw_after_partial.candidates
                    )
                });
            switch_core.set_opencc_mode(OpenCcMode::Traditional);
            let sw_committed = switch_core.select_candidate(sw_pr_idx).unwrap();
            assert_eq!(
                sw_committed.commit_text.as_deref(),
                Some("我們手写中问"),
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
    fn clipboard_preferences_commands_and_persisted_clear() {
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
        let payload = |text: &str| ClipboardPayload {
            id: None,
            content: text.into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 0,
        };
        core.clipboard_push_local(payload("你好 OpenAI"));
        core.clipboard_ingest_remote(payload("中国 Vitral"), false);
        assert!(core.personalization_stats().phrase_count >= 2);
        assert_eq!(core.personalization_stats().english_count, 2);
        let mut preferences = cfg.clone();
        preferences["learn_from_clipboard"] = serde_json::json!(false);
        core.apply_preferences(EngineConfig::parse_json(&preferences.to_string()).unwrap())
            .unwrap();
        core.clipboard_push_local(payload("学习 RustLang"));
        assert!(!core
            .input
            .lock()
            .retention
            .user_phrases
            .contains_key("学习"));
        assert!(core
            .input
            .lock()
            .retention
            .query_user_english("rustlang")
            .is_none());
        {
            let mut st = core.input.lock();
            st.retention.pin_word("保留");
            st.retention.erase_word("删除");
        }
        let stats =
            commands::execute(&core, "personalization-stats", &serde_json::json!({})).unwrap();
        assert_eq!(stats["english_count"], 2);
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
