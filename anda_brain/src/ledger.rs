//! Memory usage ledger (memory evolution plan, module M1).
//!
//! Off-graph retrieval diagnostics: which graph
//! entities each completed recall surfaced, and which were later corrected
//! (superseded). The ledger — not the graph — absorbs the high-frequency
//! writes. These counters do not reinforce the graph or calibrate utility.
//! Delivery receipts and independently qualified consequence calibration
//! live in `recall_receipt` and `consequence::utility` instead.

use anda_db::{
    collection::{Collection, CollectionConfig},
    database::AndaDB,
    error::DBError,
    query::{Filter, Fv, Query, RangeQuery},
    schema::AndaDBSchema,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Usage counters for one graph entity.
#[derive(Debug, Clone, Default, Serialize, Deserialize, AndaDBSchema)]
pub struct MemoryUsage {
    pub _id: u64,

    /// Graph entity id: `"C:<id>"` for concepts, `"P:<id>:<predicate>"`
    /// for propositions.
    pub entity: String,

    /// Completed production recalls that surfaced this entity.
    pub recall_count: u64,

    /// Recalls issued by the maintenance self-test (plan module M7). Tracked
    /// separately and never merged into `recall_count`: the brain testing
    /// itself must not count as usage (plan guardrail 1).
    pub self_test_count: u64,

    /// Receipt time of the last diagnostic test, independent of other usage.
    /// Optional: v3 added it, and a schema upgrade may only add optional
    /// fields, so a v2 ledger (Brain 0.12.1) keeps opening.
    #[serde(default)]
    pub last_self_test_at: Option<u64>,

    /// Unix ms of the newest production recall that surfaced this entity.
    pub last_recalled_at: u64,

    /// 1 once the entity has been observed superseded/corrected.
    /// `record_correction` deliberately deduplicates — repeat observations of
    /// the same supersede event must not compound the correction penalty —
    /// so despite the name this is a flag, kept as a count for schema
    /// compatibility.
    pub correction_count: u64,

    /// Unix ms when the newest correction was observed.
    pub last_corrected_at: u64,

    /// Vestigial: the `recall_count` already written back to graph metadata.
    ///
    /// Nothing writes recall counts back any more — reading must not reinforce
    /// what it read (reference Recall policy §1, §32), so the settlement's
    /// reinforcement pass was removed and this ledger became pure
    /// instrumentation. The two fields below survive for stored-record
    /// compatibility; no runtime index or write maintains them.
    pub flushed_recall_count: u64,

    /// Vestigial companion of `flushed_recall_count`; see its docs. (u64
    /// because AndaDB BTree indexes do not support Bool.)
    pub dirty: u64,

    pub updated_at: u64,
}

/// Wrapper around the `memory_usage` collection. All mutations serialize on
/// an internal lock so concurrent recall writebacks cannot race one entity
/// into duplicate rows.
pub struct UsageLedger {
    collection: Arc<Collection>,
    write_lock: tokio::sync::Mutex<()>,
}

impl UsageLedger {
    pub async fn connect(db: &Arc<AndaDB>) -> Result<Self, DBError> {
        // v3 adds an independent self-test timestamp; legacy counters stay readable.
        let mut schema = MemoryUsage::schema()?;
        schema.with_version(3);
        let collection = db
            .open_or_create_collection(
                schema,
                CollectionConfig {
                    name: "memory_usage".to_string(),
                    description: "Memory usage ledger (recall/correction counters)".to_string(),
                },
                async |collection| {
                    collection.create_btree_index_nx(&["entity"]).await?;
                    collection.remove_btree_index(&["dirty"]).await?;
                    collection
                        .create_btree_index_nx(&["last_recalled_at"])
                        .await?;
                    collection
                        .create_btree_index_nx(&["last_corrected_at"])
                        .await?;
                    Ok(())
                },
            )
            .await?;
        Ok(Self {
            collection,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn get(&self, entity: &str) -> Result<Option<MemoryUsage>, DBError> {
        let rows: Vec<MemoryUsage> = self
            .collection
            .search_as(Query {
                search: None,
                filter: Some(Filter::Field((
                    "entity".to_string(),
                    RangeQuery::Eq(Fv::Text(entity.to_string())),
                ))),
                limit: Some(1),
            })
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Records one completed recall having surfaced `entities`. Returns the
    /// number of ledger rows touched.
    pub async fn record_recall(
        &self,
        entities: &BTreeSet<String>,
        now_ms: u64,
    ) -> Result<u64, DBError> {
        let _guard = self.write_lock.lock().await;
        let mut touched = 0u64;
        for entity in entities {
            match self.get(entity).await? {
                Some(row) => {
                    self.collection
                        .update(
                            row._id,
                            BTreeMap::from([
                                ("recall_count".to_string(), Fv::U64(row.recall_count + 1)),
                                ("last_recalled_at".to_string(), Fv::U64(now_ms)),
                                ("updated_at".to_string(), Fv::U64(now_ms)),
                            ]),
                        )
                        .await?;
                }
                None => {
                    self.collection
                        .add_from(&MemoryUsage {
                            entity: entity.clone(),
                            recall_count: 1,
                            last_recalled_at: now_ms,
                            updated_at: now_ms,
                            ..Default::default()
                        })
                        .await?;
                }
            }
            touched += 1;
        }
        Ok(touched)
    }

    /// Records a correction (superseded memory) observation. Returns `true`
    /// when this is the first correction seen for the entity, so settlement
    /// can diff "newly corrected since last cycle" out of a full scan.
    pub async fn record_correction(&self, entity: &str, now_ms: u64) -> Result<bool, DBError> {
        let _guard = self.write_lock.lock().await;
        match self.get(entity).await? {
            Some(row) => {
                if row.correction_count > 0 {
                    return Ok(false);
                }
                self.collection
                    .update(
                        row._id,
                        BTreeMap::from([
                            ("correction_count".to_string(), Fv::U64(1)),
                            ("last_corrected_at".to_string(), Fv::U64(now_ms)),
                            ("updated_at".to_string(), Fv::U64(now_ms)),
                        ]),
                    )
                    .await?;
                Ok(true)
            }
            None => {
                self.collection
                    .add_from(&MemoryUsage {
                        entity: entity.to_string(),
                        correction_count: 1,
                        last_corrected_at: now_ms,
                        updated_at: now_ms,
                        ..Default::default()
                    })
                    .await?;
                Ok(true)
            }
        }
    }

    /// Records self-test retrievals (plan M7). Deliberately touches only
    /// `self_test_count` — never `recall_count`/`last_recalled_at` — so the
    /// brain testing itself can never reinforce its own memories
    /// (plan guardrail 1).
    pub async fn record_self_test(
        &self,
        entities: &BTreeSet<String>,
        now_ms: u64,
    ) -> Result<(), DBError> {
        let _guard = self.write_lock.lock().await;
        for entity in entities {
            match self.get(entity).await? {
                Some(row) => {
                    self.collection
                        .update(
                            row._id,
                            BTreeMap::from([
                                (
                                    "self_test_count".to_string(),
                                    Fv::U64(row.self_test_count + 1),
                                ),
                                ("last_self_test_at".to_string(), Fv::U64(now_ms)),
                                ("updated_at".to_string(), Fv::U64(now_ms)),
                            ]),
                        )
                        .await?;
                }
                None => {
                    self.collection
                        .add_from(&MemoryUsage {
                            entity: entity.clone(),
                            self_test_count: 1,
                            last_self_test_at: Some(now_ms),
                            updated_at: now_ms,
                            ..Default::default()
                        })
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Rows corrected after `since_ms`, newest signals for scenario mining
    /// (plan M9).
    pub async fn corrected_since(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<MemoryUsage>, DBError> {
        let rows: Vec<MemoryUsage> = self
            .collection
            .search_as(Query {
                search: None,
                filter: Some(Filter::Field((
                    "last_corrected_at".to_string(),
                    RangeQuery::Gt(Fv::U64(since_ms)),
                ))),
                limit: Some(limit),
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter(|row| row.correction_count > 0)
            .collect())
    }

    /// Removes an entity's ledger row entirely (plan M6 forget cascade).
    pub async fn forget_entity(&self, entity: &str) -> Result<bool, DBError> {
        let _guard = self.write_lock.lock().await;
        match self.get(entity).await? {
            Some(row) => {
                self.collection.remove(row._id).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// One negative-knowledge entry: a query the graph provably had nothing for.
#[derive(Debug, Clone, Default, Serialize, Deserialize, AndaDBSchema)]
pub struct RecallMiss {
    pub _id: u64,
    pub query: String,
    pub created_at: u64,
}

/// Negative-knowledge cache (plan M5): remembers probe queries that found
/// nothing, so agents stop paying to hit the same wall. Invalidation is
/// deliberately coarse — any completed formation clears the whole cache
/// (new memory could answer any past miss) — with a TTL as backstop.
///
/// Keys are normalized (whitespace-folded, lowercased): the cache exists to
/// absorb repeats, and users re-ask the same question with different casing
/// and spacing.
pub struct MissCache {
    collection: Arc<Collection>,
    write_lock: tokio::sync::Mutex<()>,
}

/// Backstop TTL for negative-knowledge entries.
pub const RECALL_MISS_TTL_MS: u64 = 3_600_000;

/// Hard row cap: unauthenticated probes on public spaces must not be able to
/// grow this collection without bound. At the cap, expired rows are purged;
/// if the cache is still full the new miss simply is not cached (a full
/// cache only costs performance, never correctness).
const MISS_CACHE_MAX_ROWS: usize = 1024;

/// Queries longer than this are never cached (they are unlikely to repeat
/// verbatim, and unbounded query text is a disk-write amplifier).
const MISS_QUERY_MAX_CHARS: usize = 512;

impl MissCache {
    pub async fn connect(db: &Arc<AndaDB>) -> Result<Self, DBError> {
        let collection = db
            .open_or_create_collection(
                RecallMiss::schema()?,
                CollectionConfig {
                    name: "recall_misses".to_string(),
                    description: "Negative-knowledge cache (queries with no memory)".to_string(),
                },
                async |collection| {
                    collection.create_btree_index_nx(&["query"]).await?;
                    Ok(())
                },
            )
            .await?;
        Ok(Self {
            collection,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Cache key: whitespace-folded, lowercased query text.
    fn cache_key(query: &str) -> String {
        query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    async fn get(&self, key: &str) -> Result<Option<RecallMiss>, DBError> {
        let rows: Vec<RecallMiss> = self
            .collection
            .search_as(Query {
                search: None,
                filter: Some(Filter::Field((
                    "query".to_string(),
                    RangeQuery::Eq(Fv::Text(key.to_string())),
                ))),
                limit: Some(1),
            })
            .await?;
        Ok(rows.into_iter().next())
    }

    /// True when a fresh (unexpired) miss is cached for this query. Expired
    /// rows are pruned lazily.
    pub async fn is_fresh_miss(&self, query: &str, now_ms: u64) -> Result<bool, DBError> {
        match self.get(&Self::cache_key(query)).await? {
            Some(row) if now_ms.saturating_sub(row.created_at) <= RECALL_MISS_TTL_MS => Ok(true),
            Some(row) => {
                let _guard = self.write_lock.lock().await;
                let _ = self.collection.remove(row._id).await;
                Ok(false)
            }
            None => Ok(false),
        }
    }

    /// Records a miss observed at `now_ms`. A cache clear racing an
    /// in-flight probe may re-cache a query that just-formed memory can now
    /// answer; that staleness only affects the probe channel and self-heals
    /// on the next formation clear or the TTL.
    pub async fn record_miss(&self, query: &str, now_ms: u64) -> Result<(), DBError> {
        let key = Self::cache_key(query);
        if key.is_empty() || key.chars().count() > MISS_QUERY_MAX_CHARS {
            return Ok(());
        }
        let _guard = self.write_lock.lock().await;
        match self.get(&key).await? {
            Some(row) => {
                self.collection
                    .update(
                        row._id,
                        BTreeMap::from([("created_at".to_string(), Fv::U64(now_ms))]),
                    )
                    .await?;
            }
            None => {
                if self.collection.len() >= MISS_CACHE_MAX_ROWS {
                    self.purge_expired_locked(now_ms).await?;
                    if self.collection.len() >= MISS_CACHE_MAX_ROWS {
                        return Ok(());
                    }
                }
                self.collection
                    .add_from(&RecallMiss {
                        query: key,
                        created_at: now_ms,
                        ..Default::default()
                    })
                    .await?;
            }
        }
        Ok(())
    }

    /// Removes every expired row. Caller must hold `write_lock`.
    async fn purge_expired_locked(&self, now_ms: u64) -> Result<(), DBError> {
        let mut cursor = 0u64;
        loop {
            let rows: Vec<RecallMiss> = self
                .collection
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(cursor)),
                    ))),
                    limit: Some(100),
                })
                .await?;
            let Some(max_id) = rows.iter().map(|row| row._id).max() else {
                break;
            };
            cursor = cursor.max(max_id);
            for row in rows {
                if now_ms.saturating_sub(row.created_at) > RECALL_MISS_TTL_MS {
                    let _ = self.collection.remove(row._id).await;
                }
            }
        }
        Ok(())
    }

    /// Drops every cached miss. Called when formation completes: any new
    /// memory could answer any past miss, and clearing is cheaper than being
    /// wrong.
    pub async fn clear(&self) -> Result<u64, DBError> {
        let _guard = self.write_lock.lock().await;
        let mut cleared = 0u64;
        let mut cursor = 0u64;
        loop {
            // Filterless queries return nothing in AndaDB; scan by `_id`.
            // The cursor advances past rows whose remove failed (same shape
            // as purge_expired_locked), so a persistent storage error cannot
            // spin this loop forever — clear() runs inline in the
            // conversation-end hook. Skipped rows expire via the TTL purge.
            let rows: Vec<RecallMiss> = self
                .collection
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(cursor)),
                    ))),
                    limit: Some(100),
                })
                .await?;
            let Some(max_id) = rows.iter().map(|row| row._id).max() else {
                break;
            };
            cursor = cursor.max(max_id);
            for row in rows {
                if self.collection.remove(row._id).await.is_ok() {
                    cleared += 1;
                }
            }
        }
        Ok(cleared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_db::database::DBConfig;

    /// The ledger row as Brain 0.12.1 stored it (schema v2).
    #[derive(Debug, Clone, Default, Serialize, Deserialize, AndaDBSchema)]
    struct MemoryUsageV2 {
        _id: u64,
        entity: String,
        recall_count: u64,
        self_test_count: u64,
        last_recalled_at: u64,
        correction_count: u64,
        last_corrected_at: u64,
        flushed_recall_count: u64,
        dirty: u64,
        updated_at: u64,
    }

    #[tokio::test]
    async fn a_v2_ledger_upgrades_in_place() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let config = DBConfig {
            name: "ledger_upgrade".into(),
            ..Default::default()
        };
        let db = Arc::new(AndaDB::create(store.clone(), config.clone()).await.unwrap());
        let mut schema = MemoryUsageV2::schema().unwrap();
        schema.with_version(2);
        let v2 = db
            .open_or_create_collection(
                schema,
                CollectionConfig {
                    name: "memory_usage".to_string(),
                    description: "Memory usage ledger (recall/correction counters)".to_string(),
                },
                async |collection| collection.create_btree_index_nx(&["entity"]).await,
            )
            .await
            .unwrap();
        v2.add_from(&MemoryUsageV2 {
            entity: "C:7".into(),
            self_test_count: 1,
            updated_at: 1,
            ..Default::default()
        })
        .await
        .unwrap();
        db.close().await.unwrap();

        let db = Arc::new(AndaDB::open(store, config).await.unwrap());
        let ledger = UsageLedger::connect(&db).await.unwrap();
        let row = ledger.get("C:7").await.unwrap().unwrap();
        assert_eq!(row.self_test_count, 1);
        assert_eq!(row.last_self_test_at, None);
        ledger
            .record_self_test(&BTreeSet::from(["C:7".to_string()]), 42)
            .await
            .unwrap();
        let row = ledger.get("C:7").await.unwrap().unwrap();
        assert_eq!((row.self_test_count, row.last_self_test_at), (2, Some(42)));
        db.close().await.unwrap();
    }
}
