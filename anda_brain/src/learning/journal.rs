//! Private orchestration state, not a learning-score database. Every update is
//! conditional and read back before dispatch. Native facts remain in Nexus.
use anda_core::BoxError;
#[cfg(feature = "learning")]
use futures::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;

pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct Journal {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
}
pub(crate) struct Versioned<T> {
    pub value: T,
    pub(crate) version: UpdateVersion,
}

impl Journal {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self {
            store,
            prefix: prefix.into(),
        }
    }

    fn path(&self, key: &str) -> Path {
        Path::from(format!("{}/{key}", self.prefix))
    }

    pub async fn read<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<Versioned<T>>, BoxError> {
        let result = match self.store.get(&self.path(key)).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if result.meta.size > MAX_BYTES as u64 {
            return Err("learning journal object exceeds bound".into());
        }
        let version = UpdateVersion {
            e_tag: result.meta.e_tag.clone(),
            version: result.meta.version.clone(),
        };
        if version.e_tag.is_none() && version.version.is_none() {
            return Err("learning journal requires conditional-update storage".into());
        }
        let bytes = result.bytes().await?;
        Ok(Some(Versioned {
            value: serde_json::from_slice(&bytes)?,
            version,
        }))
    }

    pub async fn create<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), BoxError> {
        self.put(key, value, PutMode::Create).await
    }

    #[cfg(feature = "learning")]
    pub async fn save<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        state: &Versioned<T>,
    ) -> Result<(), BoxError> {
        self.put(key, &state.value, PutMode::Update(state.version.clone()))
            .await
    }

    pub(crate) async fn put<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        value: &T,
        mode: PutMode,
    ) -> Result<(), BoxError> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_BYTES {
            return Err("learning journal object exceeds bound".into());
        }
        // No timeout/select around storage mutations. A caller cancellation is
        // detached at the runtime owner, so a write always runs to completion.
        let result = self
            .store
            .put_opts(&self.path(key), bytes.clone().into(), mode.into())
            .await;
        // Even a durable PUT whose ACK was lost is resolved by exact readback;
        // a different/newer value is never silently overwritten.
        let stored = self.store.get(&self.path(key)).await?;
        if stored.meta.size > MAX_BYTES as u64 {
            return Err("learning journal readback exceeds bound".into());
        }
        if stored.bytes().await?.as_ref() != bytes {
            return Err(result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "learning journal conditional-write conflict".into())
                .into());
        }
        Ok(())
    }

    /// One-time import of the v1 catalog, which admitted at most 32 jobs.
    /// Overflow is an error, never a claim of complete discovery.
    #[cfg(feature = "learning")]
    pub async fn legacy_jobs(&self) -> Result<Vec<String>, BoxError> {
        let prefix = self.path("jobs");
        let mut stream = self.store.list(Some(&prefix));
        let mut names = Vec::new();
        while let Some(meta) = stream.try_next().await? {
            if names.len() >= 32 {
                return Err("learning job catalog exceeds bound".into());
            }
            let name = meta
                .location
                .as_ref()
                .strip_prefix(self.prefix.as_ref())
                .ok_or("invalid learning path")?
                .trim_start_matches('/')
                .to_string();
            if !name.starts_with("jobs/") || name[5..].contains('/') {
                return Err("invalid learning job path".into());
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }
}
