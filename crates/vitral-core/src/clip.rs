use crate::error::VitralError;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

static INSTANCE_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardPayload {
    #[serde(default)]
    pub id: Option<String>,
    pub content: String,
    #[serde(default = "default_mime")]
    pub mime_type: String,
    #[serde(default)]
    pub source_device: String,
    #[serde(default)]
    pub timestamp_ms: u64,
}

fn default_mime() -> String {
    "text/plain".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardEntry {
    pub id: String,
    pub content: String,
    pub mime_type: String,
    pub source_device: String,
    pub timestamp_ms: u64,
    pub pinned: bool,
    #[serde(default, deserialize_with = "deserialize_shortcut")]
    pub shortcut_code: Option<String>,
}

/// Persistent clipboard history, independent of the current system clipboard value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardState {
    pub capacity: usize,
    pub pinned_count: usize,
    pub ring: Vec<ClipboardEntry>,
}

/// Entry IDs changed on this device since its last successful WebDAV write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RingChanges {
    changed: std::collections::BTreeSet<String>,
    removed: std::collections::BTreeSet<String>,
}

impl RingChanges {
    pub fn record(&mut self, before: &[ClipboardEntry], after: &[ClipboardEntry]) {
        for previous in before {
            if !after.iter().any(|e| e.id == previous.id) {
                self.changed.remove(&previous.id);
                self.removed.insert(previous.id.clone());
            }
        }
        for current in after {
            if !before.contains(current) {
                self.removed.remove(&current.id);
                self.changed.insert(current.id.clone());
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }

    pub fn merge(
        &self,
        cloud: Vec<ClipboardEntry>,
        local: &[ClipboardEntry],
        capacity: usize,
    ) -> Vec<ClipboardEntry> {
        let mut result: Vec<_> = local
            .iter()
            .filter(|e| self.changed.contains(&e.id))
            .cloned()
            .collect();
        result.extend(
            cloud
                .into_iter()
                .filter(|e| !self.removed.contains(&e.id) && !self.changed.contains(&e.id)),
        );
        result.sort_by_key(|e| std::cmp::Reverse(e.timestamp_ms));
        while result.len() > capacity {
            let Some(oldest) = result.iter().rposition(|e| !e.pinned) else {
                break;
            };
            result.remove(oldest);
        }
        result
    }
}

/// Current system clipboard value and independently dismissible mobile suggestion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardCurrent {
    pub current_copy: Option<ClipboardEntry>,
    pub dismissed_copy_id: Option<String>,
}

fn normalize_shortcut(code: Option<&str>) -> Result<Option<String>, VitralError> {
    let code = code.unwrap_or("").trim().to_ascii_lowercase();
    if code.is_empty() {
        return Ok(None);
    }
    if code.len() > 16 || !code.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(VitralError::InvalidArgument(
            "shortcut_code must contain 1..=16 ASCII letters".into(),
        ));
    }
    Ok(Some(code))
}

fn deserialize_shortcut<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let code = Option::<String>::deserialize(deserializer)?;
    Ok(normalize_shortcut(code.as_deref()).ok().flatten())
}

pub fn copy_order(entry: &ClipboardEntry) -> (u64, &str) {
    (entry.timestamp_ms, &entry.id)
}

/// UI visibility is local to this keyboard session; dismissal survives restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickPasteState {
    pub current_copy: Option<ClipboardEntry>,
    pub candidate_visible: bool,
    pub quick_paste: Option<ClipboardEntry>,
    pub seen_while_ime_open: bool,
    pub ime_open: bool,
    pub last_seen_copy_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickPasteConsumeResult {
    pub consumed: Option<ClipboardEntry>,
    pub state: QuickPasteState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardManager {
    capacity: usize,
    device_id: String,
    instance_seq: u64,
    next_id: u64,
    ring: VecDeque<ClipboardEntry>,
    quick_paste: Option<ClipboardEntry>,
    #[serde(default)]
    current: ClipboardCurrent,
    #[serde(default)]
    hidden_for_session: bool,
    seen_while_ime_open: bool,
    ime_open: bool,
    last_seen_copy_id: Option<String>,
}

impl ClipboardManager {
    pub fn new(capacity: usize) -> Self {
        Self::with_device(capacity, "vitral-local-device")
    }

    pub fn with_device(capacity: usize, device_id: &str) -> Self {
        let instance_seq = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
        let trimmed = device_id.trim();
        let effective_device_id = if trimmed.is_empty() {
            format!("vitral-device-{instance_seq}")
        } else {
            trimmed.to_string()
        };
        Self {
            capacity: capacity.max(1),
            device_id: effective_device_id,
            instance_seq,
            next_id: 1,
            ring: VecDeque::new(),
            quick_paste: None,
            current: ClipboardCurrent::default(),
            hidden_for_session: false,
            seen_while_ime_open: false,
            ime_open: false,
            last_seen_copy_id: None,
        }
    }

    fn alloc_entry(&mut self, payload: ClipboardPayload) -> ClipboardEntry {
        let explicit_id = payload.id.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let id = explicit_id.unwrap_or_else(|| {
            let seq = self.next_id;
            self.next_id += 1;
            format!(
                "{}-p{}i{}-{}-{}",
                self.device_id,
                std::process::id(),
                self.instance_seq,
                payload.timestamp_ms,
                seq
            )
        });
        ClipboardEntry {
            id,
            content: payload.content,
            mime_type: payload.mime_type,
            source_device: payload.source_device,
            timestamp_ms: payload.timestamp_ms,
            pinned: false,
            shortcut_code: None,
        }
    }

    fn push_ring_entry(&mut self, mut entry: ClipboardEntry) {
        entry.shortcut_code = normalize_shortcut(entry.shortcut_code.as_deref()).ok().flatten();
        if self.ring.iter().any(|e| e.id == entry.id) {
            return;
        }
        while self.ring.len() >= self.capacity {
            if let Some(pos) = self.ring.iter().rposition(|e| !e.pinned) {
                self.ring.remove(pos);
            } else {
                break;
            }
        }
        if self.ring.len() < self.capacity {
            self.ring.push_front(entry);
        }
    }

    pub fn current(&self) -> &ClipboardCurrent { &self.current }

    pub fn restore_current(&mut self, state: ClipboardCurrent) {
        self.current = state;
        self.refresh_quick_paste();
    }

    fn refresh_quick_paste(&mut self) {
        self.last_seen_copy_id = self.current.current_copy.as_ref().map(|e| e.id.clone());
        self.quick_paste = self.current.current_copy.as_ref().filter(|e|
            self.current.dismissed_copy_id.as_deref() != Some(e.id.as_str())).cloned();
    }

    pub fn observe_current(&mut self, mut entry: ClipboardEntry) -> bool {
        entry.shortcut_code = normalize_shortcut(entry.shortcut_code.as_deref()).ok().flatten();
        if self.current.current_copy.as_ref().is_none_or(|old| copy_order(&entry) > copy_order(old)) {
            self.current.current_copy = Some(entry);
            self.current.dismissed_copy_id = None;
            self.refresh_quick_paste();
            return true;
        }
        false
    }

    pub fn push(&mut self, payload: ClipboardPayload) -> ClipboardState {
        let entry = self.alloc_entry(payload);
        self.observe_current(entry.clone());
        self.push_ring_entry(entry);
        self.state()
    }

    pub fn ingest_remote(&mut self, payload: ClipboardPayload, _is_cold_boot: bool) -> QuickPasteState {
        let entry = self.alloc_entry(payload);
        self.observe_current(entry.clone());
        self.push_ring_entry(entry);
        self.quick_paste_state()
    }

    pub fn quick_paste_state(&self) -> QuickPasteState {
        QuickPasteState {
            current_copy: self.current.current_copy.clone(),
            candidate_visible: self.visible_quick_paste().is_some(),
            quick_paste: self.quick_paste.clone(),
            seen_while_ime_open: self.seen_while_ime_open,
            ime_open: self.ime_open,
            last_seen_copy_id: self.last_seen_copy_id.clone(),
        }
    }

    pub(crate) fn visible_quick_paste(&self) -> Option<&ClipboardEntry> {
        self.quick_paste.as_ref().filter(|_| self.ime_open && !self.hidden_for_session)
    }

    pub(crate) fn quick_paste_id(&self) -> Option<&str> {
        self.quick_paste.as_ref().map(|entry| entry.id.as_str())
    }

    pub fn consume_quick_paste(&mut self) -> QuickPasteConsumeResult {
        let consumed = self.quick_paste.clone();
        self.dismiss_quick_paste();
        QuickPasteConsumeResult { consumed, state: self.quick_paste_state() }
    }

    pub fn hide_for_input(&mut self) { self.hidden_for_session = true; }

    pub fn on_ime_open(&mut self) -> QuickPasteState {
        if !self.ime_open { self.hidden_for_session = false; }
        self.ime_open = true;
        self.seen_while_ime_open = self.quick_paste.is_some();
        self.quick_paste_state()
    }

    pub fn on_ime_close(&mut self) -> QuickPasteState {
        self.ime_open = false;
        self.seen_while_ime_open = false;
        self.hidden_for_session = false;
        self.quick_paste_state()
    }

    pub fn dismiss_quick_paste(&mut self) -> QuickPasteState {
        self.current.dismissed_copy_id = self.current.current_copy.as_ref().map(|e| e.id.clone());
        self.refresh_quick_paste();
        self.seen_while_ime_open = false;
        self.quick_paste_state()
    }

    pub fn pin(
        &mut self,
        item_id: &str,
        shortcut_code: Option<&str>,
    ) -> Result<ClipboardState, VitralError> {
        let normalized_code = normalize_shortcut(shortcut_code)?;
        let item = self
            .ring
            .iter_mut()
            .find(|e| e.id == item_id)
            .ok_or_else(|| {
                VitralError::NotFound(format!("clipboard item '{item_id}' not found"))
            })?;
        item.pinned = true;
        item.shortcut_code = normalized_code;
        Ok(self.state())
    }

    pub fn unpin(&mut self, item_id: &str) -> Result<ClipboardState, VitralError> {
        let item = self
            .ring
            .iter_mut()
            .find(|e| e.id == item_id)
            .ok_or_else(|| {
                VitralError::NotFound(format!("clipboard item '{item_id}' not found"))
            })?;
        item.pinned = false;
        item.shortcut_code = None;
        Ok(self.state())
    }

    pub fn clear_unpinned(&mut self) -> ClipboardState {
        self.ring.retain(|e| e.pinned);
        self.state()
    }

    pub fn find_shortcut(&self, code: &str) -> Option<&ClipboardEntry> {
        self.ring
            .iter()
            .find(|e| e.pinned && e.shortcut_code.as_deref() == Some(code))
    }

    pub fn state(&self) -> ClipboardState {
        let mut ring: Vec<ClipboardEntry> = self.ring.iter().cloned().collect();
        ring.sort_by_key(|a| std::cmp::Reverse(a.pinned));
        let pinned_count = ring.iter().filter(|e| e.pinned).count();
        ClipboardState {
            capacity: self.capacity,
            pinned_count,
            ring,
        }
    }

    pub fn export_ring(&self) -> Vec<ClipboardEntry> {
        self.ring.iter().cloned().collect()
    }

    pub(crate) fn ring_matches(&self, entries: &[ClipboardEntry]) -> bool {
        self.ring.iter().eq(entries)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.ring.len() > self.capacity {
            if let Some(pos) = self.ring.iter().rposition(|e| !e.pinned) {
                self.ring.remove(pos);
            } else {
                break;
            }
        }
    }

    /// History edits never consume or replace the separately persisted current copy.
    pub fn observe_cloud_ring(&mut self, entries: Vec<ClipboardEntry>, _first_read: bool) -> bool {
        let current_changed = entries.iter().max_by_key(|e| copy_order(e))
            .is_some_and(|newest| self.observe_current(newest.clone()));
        self.ring.clear();
        for entry in entries.into_iter().rev() { self.push_ring_entry(entry); }
        current_changed
    }

    pub fn restore_ring_cold_boot(&mut self, entries: Vec<ClipboardEntry>) {
        self.observe_cloud_ring(entries, true);
        self.seen_while_ime_open = false;
    }
}
