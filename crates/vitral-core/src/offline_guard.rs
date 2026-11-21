use crate::error::VitralError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Centralized physical egress gate checked before any WAN/AI/WebDAV/relay network action.
#[derive(Debug, Clone)]
pub struct OfflineGuard {
    pure_offline: Arc<AtomicBool>,
}

impl OfflineGuard {
    pub fn new(initial: bool) -> Self {
        Self {
            pure_offline: Arc::new(AtomicBool::new(initial)),
        }
    }

    pub fn set_pure_offline(&self, enabled: bool) {
        self.pure_offline.store(enabled, Ordering::SeqCst);
    }

    pub fn is_pure_offline(&self) -> bool {
        self.pure_offline.load(Ordering::SeqCst)
    }

    pub fn require_online(&self, operation: &str) -> Result<(), VitralError> {
        if self.is_pure_offline() {
            return Err(VitralError::PureOfflineViolation(format!(
                "blocked outbound operation '{operation}' while pure_offline=true"
            )));
        }
        Ok(())
    }
}
