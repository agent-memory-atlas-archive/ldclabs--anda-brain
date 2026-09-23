//! Private orchestration state, not a learning-score database. Every update is
//! conditional and read back before dispatch. Native facts remain in Nexus.
use anda_core::BoxError;
#[cfg(feature = "learning")]
use futures::TryStreamExt;
use futures::{StreamExt, stream::BoxStream};
use object_store::{ObjectStore, PutMode, path::Path};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;

pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct Journal {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
}
pub(crate) use crate::persisted::Versioned;

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

    pub(crate) fn keys(&self, prefix: &str) -> BoxStream<'_, Result<String, BoxError>> {
        let root = format!("{}/", self.prefix);
        self.store
            .list(Some(&self.path(prefix)))
            .map(move |entry| {
                let entry = entry?;
                entry
                    .location
                    .as_ref()
                    .strip_prefix(&root)
                    .map(str::to_string)
                    .ok_or_else(|| "invalid journal path".into())
            })
            .boxed()
    }

    pub async fn read<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<Versioned<T>>, BoxError> {
        crate::persisted::read(
            self.store.as_ref(),
            &self.path(key),
            MAX_BYTES as u64,
            "learning journal",
        )
        .await
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
        crate::persisted::put(
            self.store.as_ref(),
            &self.path(key),
            value,
            mode,
            MAX_BYTES as u64,
            "learning journal",
        )
        .await
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
