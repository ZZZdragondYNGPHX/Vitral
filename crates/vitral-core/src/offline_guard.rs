use crate::error::VitralError;
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Centralized physical egress gate checked before any WAN/AI/WebDAV/relay network action.
#[derive(Debug, Clone)]
pub struct OfflineGuard {
    state: Arc<EgressState>,
}

#[derive(Debug)]
struct EgressState {
    pure_offline: AtomicBool,
    transition: Mutex<()>,
    online_gate: RwLock<()>,
}

impl OfflineGuard {
    pub fn new(initial: bool) -> Self {
        Self {
            state: Arc::new(EgressState {
                pure_offline: AtomicBool::new(initial),
                transition: Mutex::new(()),
                online_gate: RwLock::new(()),
            }),
        }
    }

    pub fn set_pure_offline(&self, enabled: bool) {
        self.set_pure_offline_with(enabled, |_| {});
    }

    pub(crate) fn set_pure_offline_with(&self, enabled: bool, on_enable: impl FnOnce(bool)) {
        let _transition = self.state.transition.lock();
        let was_offline = self.state.pure_offline.swap(enabled, Ordering::SeqCst);
        if enabled {
            on_enable(was_offline);
            // Existing requests observe the flag and drop their futures before releasing the gate.
            let _drained = self.state.online_gate.write();
        }
    }

    #[cfg(test)]
    pub(crate) fn has_online_operation(&self) -> bool {
        self.state.online_gate.is_locked()
    }

    pub fn is_pure_offline(&self) -> bool {
        self.state.pure_offline.load(Ordering::SeqCst)
    }

    pub(crate) fn enter_online(
        &self,
        operation: &str,
    ) -> Result<RwLockReadGuard<'_, ()>, VitralError> {
        self.require_online(operation)?;
        let online = self.state.online_gate.read();
        self.require_online(operation)?;
        Ok(online)
    }

    /// Poll an in-flight operation while its caller holds the online gate.
    pub(crate) async fn wait_online<T>(
        &self,
        operation: &str,
        future: impl Future<Output = Result<T, VitralError>>,
        check_cancel: impl Fn() -> Result<(), VitralError>,
    ) -> Result<T, VitralError> {
        let mut pending = std::pin::pin!(future);
        loop {
            self.require_online(operation)?;
            check_cancel()?;
            // Keep polling the same future and drop it before the caller releases the gate.
            if let Ok(result) = tokio::time::timeout(Duration::from_millis(25), &mut pending).await
            {
                self.require_online(operation)?;
                return result;
            }
        }
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
