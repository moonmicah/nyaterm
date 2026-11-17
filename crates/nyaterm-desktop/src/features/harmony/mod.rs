//! HarmonyOS device support: hdc subprocess bindings, terminal sessions and the
//! file browser bound to the same call.
use nyaterm_files::hdc::HdcFileService;
use std::collections::HashMap;
use std::sync::Arc;

pub(in crate::features) mod labels;
mod runtime;

pub(in crate::features) struct HarmonyFeatureState {
    /// Saved-connection id → in-flight start request id.
    pub pending: HashMap<String, String>,
    /// Saved-connection id → file browser for the bound device.
    ///
    /// File services are connection-scoped like Android's device leases, so a
    /// reconnect replaces the entry while an open pane keeps its own handle.
    files: HashMap<String, Arc<HdcFileService>>,
}

impl HarmonyFeatureState {
    pub(in crate::features) fn new() -> Self {
        Self {
            pending: HashMap::new(),
            files: HashMap::new(),
        }
    }

    pub(in crate::features) fn register_files(&mut self, key: &str, service: Arc<HdcFileService>) {
        self.files.insert(key.to_string(), service);
    }

    pub(in crate::features) fn file_service(
        &self,
        key: &str,
    ) -> anyhow::Result<Arc<HdcFileService>> {
        self.files
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::Error::new(nyaterm_harmony::error::HdcError::Offline))
    }
}
