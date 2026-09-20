use super::*;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(super) const MAX_SLOTS: u64 = 1_000_000;
const MAX_BYTES: u64 = 128 * 1024;

pub(crate) struct Directory {
    store: Arc<dyn ObjectStore>,
    pub shard: u32,
    gate: Mutex<()>,
    pub tick_gate: Mutex<()>,
    pub tasks: crate::runtime::DurableTasks,
    pub(super) semantic_slots: Arc<tokio::sync::Semaphore>,
}
pub(crate) struct Versioned<T> {
    pub value: T,
    pub(crate) version: UpdateVersion,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Root {
    format: String,
    allocated: u64,
    cursor: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format: String,
    scope: RuntimeScope,
    shard: u32,
}

impl Directory {
    pub fn new(store: Arc<dyn ObjectStore>, shard: u32) -> Arc<Self> {
        Arc::new(Self {
            store,
            shard,
            gate: Mutex::new(()),
            tick_gate: Mutex::new(()),
            tasks: Default::default(),
            semantic_slots: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }
    fn path(&self, key: &str) -> Path {
        format!("__brain_runtime__/v1/shards/{}/{key}", self.shard).into()
    }
    fn entry_key(id: &str) -> Result<String, BoxError> {
        valid_id(id)?;
        Ok(format!(
            "spaces/{}",
            &anda_cognitive_nexus::content_digest(&serde_json::json!(id))?[7..]
        ))
    }
    pub(crate) async fn read<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<Versioned<T>>, BoxError> {
        let result = match self.store.get(&self.path(key)).await {
            Ok(v) => v,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if result.meta.size > MAX_BYTES {
            return Err("attention object exceeds bound".into());
        }
        let version = UpdateVersion {
            e_tag: result.meta.e_tag.clone(),
            version: result.meta.version.clone(),
        };
        if version.e_tag.is_none() && version.version.is_none() {
            return Err("attention requires conditional-update storage".into());
        }
        Ok(Some(Versioned {
            value: serde_json::from_slice(&result.bytes().await?)?,
            version,
        }))
    }
    pub(crate) async fn put<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        mode: PutMode,
    ) -> Result<(), BoxError> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("attention object exceeds bound".into());
        }
        let result = self
            .store
            .put_opts(&self.path(key), bytes.clone().into(), mode.into())
            .await;
        let saved = self.store.get(&self.path(key)).await?;
        if saved.meta.size > MAX_BYTES || saved.bytes().await?.as_ref() != bytes {
            return Err(result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "attention conditional-write conflict".into())
                .into());
        }
        Ok(())
    }
    async fn root(&self) -> Result<Versioned<Root>, BoxError> {
        if self.read::<Root>("root").await?.is_none() {
            self.put(
                "root",
                &Root {
                    format: DIRECTORY_FORMAT.into(),
                    ..Default::default()
                },
                PutMode::Create,
            )
            .await?;
        }
        let root = self
            .read::<Root>("root")
            .await?
            .ok_or("attention root unavailable")?;
        if root.value.format != DIRECTORY_FORMAT
            || root.value.allocated > MAX_SLOTS
            || root.value.cursor > root.value.allocated
        {
            return Err("invalid attention root".into());
        }
        Ok(root)
    }
    pub(super) async fn entry(&self, id: &str) -> Result<Option<Versioned<Entry>>, BoxError> {
        let result = self.read::<Entry>(&Self::entry_key(id)?).await?;
        if let Some(row) = &result {
            row.value.validate(id, self.shard)?;
        }
        Ok(result)
    }
    /// Dense, persistent slot coordinates avoid assuming ObjectStore list order
    /// (local files and remote stores need not return sorted listing pages).
    /// An interrupted allocation may leave an empty slot; no work can arm until
    /// its index and registration have both been durably acknowledged.
    pub async fn register(
        &self,
        id: &str,
        native: &anda_cognitive_nexus::attention::AttentionConfig,
        enabled: bool,
    ) -> Result<Entry, BoxError> {
        let _g = self.gate.lock().await;
        let key = Self::entry_key(id)?;
        let old = self.entry(id).await?;
        let (mut entry, mode) = if let Some(old) = old {
            if old.value.native_scope != native.scope {
                return Err("attention instance mismatch; explicit migration required".into());
            }
            (old.value, PutMode::Update(old.version))
        } else {
            let mut root = self.root().await?;
            if root.value.allocated >= MAX_SLOTS {
                return Err("attention directory is full".into());
            }
            root.value.allocated += 1;
            self.put("root", &root.value, PutMode::Update(root.version))
                .await?;
            (
                Entry {
                    format: DIRECTORY_FORMAT.into(),
                    slot: root.value.allocated,
                    registration: Registration {
                        format: FORMAT.into(),
                        scope: RuntimeScope {
                            space_id: id.into(),
                            space_instance: native.scope.space_instance.clone(),
                        },
                        pins: native.pins.clone(),
                        version: 0,
                        shard: self.shard,
                        enabled,
                        dirty_generation: 0,
                        reconciled_generation: 0,
                        next_check_ms: 0,
                    },
                    native_scope: native.scope.clone(),
                    checkpoint: Default::default(),
                    last_scan_ms: 0,
                    failures: 0,
                    last_report: Default::default(),
                    rechecks: vec![],
                },
                PutMode::Create,
            )
        };
        entry.registration.version = next(entry.registration.version)?;
        entry.registration.dirty_generation = next(entry.registration.dirty_generation)?;
        entry.registration.pins = native.pins.clone();
        entry.registration.next_check_ms = 0;
        entry.failures = 0;
        entry.checkpoint = Checkpoint::default();
        self.put(&key, &entry, mode).await?;
        self.put(
            &format!("index/{:020}", entry.slot),
            &Index {
                format: DIRECTORY_FORMAT.into(),
                scope: entry.registration.scope.clone(),
                shard: self.shard,
            },
            PutMode::Create,
        )
        .await?;
        Ok(entry)
    }
    pub async fn page(&self, limit: usize) -> Result<Vec<u64>, BoxError> {
        let _g = self.gate.lock().await;
        // No root means no registered work, not a request to initialize a store.
        if self.read::<Root>("root").await?.is_none() {
            return Ok(vec![]);
        }
        let root = self.root().await?;
        let start = if root.value.cursor == root.value.allocated {
            1
        } else {
            root.value.cursor + 1
        };
        let end = root
            .value
            .allocated
            .min(start.saturating_add(limit as u64).saturating_sub(1));
        Ok((start..=end).collect())
    }
    pub async fn read_slot(&self, slot: u64) -> Result<Option<Entry>, BoxError> {
        let Some(index) = self.read::<Index>(&format!("index/{slot:020}")).await? else {
            return Ok(None);
        };
        if index.value.format != DIRECTORY_FORMAT || index.value.shard != self.shard {
            return Err("invalid attention slot".into());
        }
        let entry = self
            .entry(&index.value.scope.space_id)
            .await?
            .ok_or("attention registration missing")?
            .value;
        if entry.slot != slot || entry.registration.scope != index.value.scope {
            return Err("attention slot identity mismatch".into());
        }
        Ok(Some(entry))
    }
    pub async fn advance_cursor(&self, slot: u64) -> Result<(), BoxError> {
        let _g = self.gate.lock().await;
        let mut root = self.root().await?;
        if slot == 0 || slot > root.value.allocated {
            return Err("invalid attention scan coordinate".into());
        }
        root.value.cursor = slot;
        self.put("root", &root.value, PutMode::Update(root.version))
            .await
    }
    /// A stale/partial scan never acknowledges a newer dirty generation.
    pub async fn finish(&self, mut entry: Entry) -> Result<bool, BoxError> {
        let _g = self.gate.lock().await;
        let id = &entry.registration.scope.space_id;
        let old = self
            .entry(id)
            .await?
            .ok_or("attention registration missing")?;
        if old.value.registration.version != entry.registration.version
            || old.value.registration.dirty_generation != entry.registration.dirty_generation
        {
            return Ok(false);
        }
        entry.registration.version = next(entry.registration.version)?;
        if entry.last_report.scan_complete {
            entry.registration.reconciled_generation = entry.registration.dirty_generation;
        }
        self.put(&Self::entry_key(id)?, &entry, PutMode::Update(old.version))
            .await?;
        Ok(true)
    }
    pub async fn edit(
        &self,
        id: &str,
        change: impl FnOnce(&mut Entry) -> Result<(), BoxError>,
    ) -> Result<(), BoxError> {
        let _g = self.gate.lock().await;
        let mut old = self
            .entry(id)
            .await?
            .ok_or("attention registration missing")?;
        change(&mut old.value)?;
        old.value.registration.version = next(old.value.registration.version)?;
        old.value.registration.dirty_generation = next(old.value.registration.dirty_generation)?;
        old.value.registration.next_check_ms = 0;
        old.value.failures = 0;
        self.put(
            &Self::entry_key(id)?,
            &old.value,
            PutMode::Update(old.version),
        )
        .await
    }
}
