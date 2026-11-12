//! HarmonyOS device support: hdc subprocess bindings and terminal sessions.
use std::collections::HashMap;

pub(in crate::features) mod labels;
mod runtime;

pub(in crate::features) struct HarmonyFeatureState {
    /// Saved-connection id → in-flight start request id.
    pub pending: HashMap<String, String>,
}

impl HarmonyFeatureState {
    pub(in crate::features) fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }
}
