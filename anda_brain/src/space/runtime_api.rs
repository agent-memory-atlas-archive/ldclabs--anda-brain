use super::*;
use crate::runtime_api::{MemoryRuntime, MemoryRuntimeBindings, config::RuntimeConfig};

impl AppState {
    pub fn with_memory_runtime_bindings(
        mut self,
        bindings: MemoryRuntimeBindings,
    ) -> Result<Self, BoxError> {
        bindings.validate()?;
        if Arc::strong_count(&self.spaces) != 1
            || !self
                .spaces
                .try_read()
                .map_err(|_| "host is in use")?
                .is_empty()
        {
            return Err("configure runtime bindings before sharing/loading Spaces".into());
        }
        if self.action_bindings.is_some()
            && bindings
                .spaces
                .values()
                .any(|s| s.actions.is_some() || s.inbox.is_some())
        {
            return Err("global and per-Space action adapters cannot both be installed".into());
        }
        self.memory_runtime_bindings = Arc::new(bindings);
        Ok(self)
    }
    /// `config` is trusted deployment data, never an HTTP/model request body.
    pub fn with_runtime_config(
        self,
        config: RuntimeConfig,
        resolve_secret: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, BoxError> {
        self.with_memory_runtime_bindings(config.resolve(resolve_secret)?)
    }
    pub(crate) fn runtime_cwt_verifier_enabled(&self) -> bool {
        !self.ed25519_pubkeys.is_empty()
    }
}
impl Space {
    pub fn trust(&self) -> Arc<crate::consequence::trust::TrustRuntime> {
        self.trust.clone()
    }
    pub(crate) async fn trust_changed(&self) -> Result<(), BoxError> {
        self.miss_cache.clear().await?;
        self.attention.register_work().await?;
        Ok(())
    }

    pub fn utility(&self) -> Arc<crate::consequence::utility::UtilityRuntime> {
        self.utility.clone()
    }
    pub fn recall_receipts(&self) -> Arc<crate::recall_receipt::RecallReceipts> {
        self.recall_receipts.clone()
    }
    pub fn memory_runtime(&self) -> Option<Arc<MemoryRuntime>> {
        self.memory_runtime.clone()
    }
}
