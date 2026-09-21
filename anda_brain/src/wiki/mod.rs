//! Anda Wiki: the semantic reference-memory layer of a Space.
//!
//! Git-model document store over AndaDB: immutable version commits with CAS
//! concurrency control, a chunk retrieval plane queried by native BM25
//! (jieba tokenizer), verifiable byte-range citations, and an append-only
//! audit log. The query path is deterministic and LLM-free; answer
//! composition belongs to the calling agent.

mod chunk;
mod digest;
/// Retrieval-quality baseline harness; exercised only by its own tests
/// (the hit-rate test *is* the harness runner).
#[cfg(test)]
mod evalset;
mod model;
mod okf;
mod outline;
mod read;
mod search;
mod store;
mod tool;
mod types;

use store::prepare_commit;

#[cfg(test)]
mod regressions;

pub use chunk::{CHUNKER_VERSION, chunk_markdown, normalize_content, slugify, slugify_path};
pub use digest::{WikiDigest, WikiDigestReport};
pub use model::*;
pub use tool::{WikiCommitTool, WikiReadTool, WikiSearchTool, WikiToolScope};
pub use types::*;

use anda_db::{
    collection::{Collection, CollectionConfig},
    database::AndaDB,
    error::DBError,
    query::{Filter, Fv, Query, RangeQuery, Search},
    schema::Json,
};
use anda_db_tfs::jieba_tokenizer;
use std::{collections::BTreeMap, sync::Arc};

use chunk::{checksum_for, chunk_checksum, floor_char_boundary, quote_excerpt};

/// Reject documents larger than this after normalization (1 MiB).
pub const MAX_DOC_BYTES: usize = 1024 * 1024;
/// `Full` reads return at most this many bytes (truncated on a char boundary).
pub const MAX_READ_BYTES: usize = 256 * 1024;
/// Initializing documents (`current_version == 0`) older than this are
/// reclaimed by the orphan sweep.
const SENTINEL_TTL_MS: u64 = 10 * 60 * 1000;
/// Page size for internal full-collection scans.
const SCAN_PAGE: usize = 200;
/// Cap for `Include` filter key lists (AndaDB rejects larger ones).
const MAX_INCLUDE_KEYS: usize = 4096;
/// Default audit-log retention (PRD §3.4).
pub const DEFAULT_EVENT_RETENTION: usize = 100_000;
/// Documents untouched for this long count as stale (PRD §7.4).
pub const DEFAULT_STALE_AFTER_MS: u64 = 180 * 24 * 3600 * 1000;

#[derive(Clone)]
pub struct WikiService {
    space_id: String,
    docs: Arc<Collection>,
    versions: Arc<Collection>,
    chunks: Arc<Collection>,
    events: Arc<Collection>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    writes: crate::runtime::DurableTasks,
    /// When set, scoped (HTTP/MCP) reads are evented; agent reads stay
    /// covered by the recall conversation log (PRD §3.4).
    audit_reads: Arc<std::sync::atomic::AtomicBool>,
}

impl WikiService {
    pub async fn connect(space_id: String, db: Arc<AndaDB>) -> Result<Self, DBError> {
        let docs = db
            .open_or_create_collection(
                WikiDocRecord::schema()?,
                CollectionConfig {
                    name: "wiki_docs".to_string(),
                    description: "Wiki document registry".to_string(),
                },
                async |c| init_wiki_docs(c).await,
            )
            .await?;
        let versions = db
            .open_or_create_collection(
                WikiVersionRecord::schema()?,
                CollectionConfig {
                    name: "wiki_versions".to_string(),
                    description: "Immutable wiki version commits".to_string(),
                },
                async |c| init_wiki_versions(c).await,
            )
            .await?;
        let chunks = db
            .open_or_create_collection(
                WikiChunkRecord::schema()?,
                CollectionConfig {
                    name: "wiki_chunks".to_string(),
                    description: "Wiki retrieval chunks".to_string(),
                },
                async |c| init_wiki_chunks(c).await,
            )
            .await?;
        let events = db
            .open_or_create_collection(
                WikiEventRecord::schema()?,
                CollectionConfig {
                    name: "wiki_events".to_string(),
                    description: "Wiki audit events".to_string(),
                },
                async |c| init_wiki_events(c).await,
            )
            .await?;

        Ok(Self {
            space_id,
            docs,
            versions,
            chunks,
            events,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            writes: crate::runtime::DurableTasks::default(),
            audit_reads: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub fn is_busy(&self) -> bool {
        self.writes.is_busy()
    }

    pub async fn shutdown(&self) {
        self.writes.shutdown().await;
    }

    /// A dropped HTTP/model waiter does not interrupt a native wiki write.
    async fn owned<T: Send + 'static>(
        &self,
        work: impl std::future::Future<Output = Result<T, WikiError>> + Send + 'static,
    ) -> Result<T, WikiError> {
        self.writes
            .run(async move { work.await.map_err(anda_core::BoxError::from) })
            .await
            .map_err(|err| match err.downcast::<WikiError>() {
                Ok(err) => *err,
                Err(err) => WikiError::Db(err.to_string()),
            })
    }

    pub fn set_audit_reads(&self, enabled: bool) {
        self.audit_reads
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn audit_reads(&self) -> bool {
        self.audit_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn docs_count(&self) -> usize {
        self.docs.metadata().stats.num_documents as usize
    }

    pub fn chunks_count(&self) -> usize {
        self.chunks.metadata().stats.num_documents as usize
    }

    pub async fn list_events(
        &self,
        kind: Option<String>,
        doc_id: Option<u64>,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<WikiEventListOutput, WikiError> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let cursor = self.cursor_or_max(&self.events, &cursor)?;
        let mut filters: Vec<Box<Filter>> = vec![Box::new(Filter::Field((
            "_id".to_string(),
            RangeQuery::Lt(Fv::U64(cursor)),
        )))];
        if let Some(kind) = kind {
            filters.push(Box::new(Filter::Field((
                "kind".to_string(),
                RangeQuery::Eq(Fv::Text(kind)),
            ))));
        }
        if let Some(doc_id) = doc_id {
            filters.push(Box::new(Filter::Field((
                "doc_id".to_string(),
                RangeQuery::Eq(Fv::U64(doc_id)),
            ))));
        }
        let rows: Vec<WikiEventRecord> =
            query_last_as(&self.events, Filter::And(filters), limit).await?;
        let next_cursor = page_cursor(&rows, limit, |e| e._id);
        Ok(WikiEventListOutput {
            events: rows
                .into_iter()
                .map(|e| WikiEventInfo {
                    id: e._id,
                    kind: e.kind,
                    doc_id: e.doc_id,
                    version_id: e.version_id,
                    actor: e.actor,
                    detail: e.detail,
                    created_at: e.created_at,
                })
                .collect(),
            next_cursor,
        })
    }

    // ─── Scoped (external) read paths ───────────────────────────────────────

    // ─── ACL defaults, counters, housekeeping ───────────────────────────────

    /// Namespace → default ACL label map, applied to newly created documents
    /// that do not set a label explicitly.
    pub async fn set_acl_defaults(
        &self,
        defaults: BTreeMap<String, String>,
    ) -> Result<(), WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.set_acl_defaults_inner(defaults).await })
            .await
    }

    async fn set_acl_defaults_inner(
        &self,
        defaults: BTreeMap<String, String>,
    ) -> Result<(), WikiError> {
        let defaults: BTreeMap<String, String> = defaults
            .into_iter()
            .map(|(ns, label)| (ns.trim().to_string(), label.trim().to_string()))
            .filter(|(ns, _)| !ns.is_empty())
            .collect();
        self.docs
            .save_extension(
                "wiki_acl_defaults".to_string(),
                serde_json::to_value(&defaults)
                    .map_err(|err| WikiError::Db(err.to_string()))?
                    .into(),
            )
            .await?;
        Ok(())
    }

    fn acl_defaults(&self) -> BTreeMap<String, String> {
        self.docs
            .get_extension_as::<BTreeMap<String, String>>("wiki_acl_defaults")
            .unwrap_or_default()
    }

    fn acl_default_for(&self, namespace: &str) -> String {
        self.acl_defaults()
            .get(namespace)
            .cloned()
            .unwrap_or_default()
    }

    pub fn versions_count(&self) -> usize {
        self.versions.metadata().stats.num_documents as usize
    }

    pub fn queries_count(&self) -> u64 {
        self.docs
            .get_extension_as::<u64>("wiki_query_count")
            .unwrap_or_default()
    }

    fn bump_query_count(&self) {
        let _ = self
            .docs
            .set_extension_from_with::<_, u64>("wiki_query_count".to_string(), |v| {
                Some(v.unwrap_or_default().saturating_add(1))
            });
    }

    pub fn stale_report_cached(&self) -> WikiStaleReport {
        self.docs
            .get_extension_as::<WikiStaleReport>("wiki_stale_report")
            .unwrap_or_default()
    }

    /// Trims the audit log to the retention cap, oldest first. The prune
    /// itself is evented so the gap is explained in the remaining log. The
    /// newest `DigestExtracted` event per document is exempt: it is the
    /// digest ledger superseding depends on (PRD §7.3) and must survive
    /// retention in long-lived spaces with rarely-updated documents.
    pub async fn prune_events(&self, max_keep: usize, now_ms: u64) -> Result<usize, WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.prune_events_inner(max_keep, now_ms).await })
            .await
    }

    async fn prune_events_inner(&self, max_keep: usize, now_ms: u64) -> Result<usize, WikiError> {
        // One batched scan up front instead of two queries per candidate:
        // the newest `DigestExtracted` id per document is the ledger head
        // that must survive retention. Concurrent digests may mint newer
        // heads mid-prune; that only makes this set conservative (an extra
        // row survives until the next prune).
        let ledger_heads = self.digest_ledger_heads().await?;
        let mut removed = 0usize;
        let mut kept_ledger = 0usize;
        let mut cursor = 0u64;
        loop {
            let total = self.events.metadata().stats.num_documents as usize;
            if total <= max_keep + kept_ledger {
                break;
            }
            let excess = (total - max_keep - kept_ledger).min(Collection::MAX_SEARCH_LIMIT);
            // Gt(cursor) + no Lt: ascending ids, i.e. the oldest entries
            // first, skipping ledger rows already visited.
            let ids = self
                .events
                .search_ids(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(cursor)),
                    ))),
                    limit: Some(excess),
                })
                .await?;
            if ids.is_empty() {
                break;
            }
            for id in ids {
                cursor = cursor.max(id);
                if ledger_heads.contains(&id) {
                    kept_ledger += 1;
                    continue;
                }
                self.events.remove(id).await?;
                removed += 1;
            }
        }
        if removed > 0 {
            self.write_event(
                EVENT_EVENTS_PRUNED,
                None,
                None,
                "system".to_string(),
                BTreeMap::from([("removed".to_string(), Json::from(removed as u64))]),
                now_ms,
            )
            .await?;
        }
        Ok(removed)
    }

    /// The newest `DigestExtracted` event id per document — the ledger
    /// entries [`WikiDigest`] reads to supersede stale facts; retention
    /// pruning must not delete them (PRD §7.3).
    async fn digest_ledger_heads(&self) -> Result<std::collections::BTreeSet<u64>, WikiError> {
        let mut newest: BTreeMap<u64, u64> = BTreeMap::new(); // doc_id → event id
        let mut cursor = self.events.max_document_id() + 1;
        loop {
            let rows: Vec<WikiEventRecord> = query_last_as(
                &self.events,
                Filter::And(vec![
                    Box::new(Filter::Field((
                        "kind".to_string(),
                        RangeQuery::Eq(Fv::Text(EVENT_DIGEST_EXTRACTED.to_string())),
                    ))),
                    Box::new(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Lt(Fv::U64(cursor)),
                    ))),
                ]),
                Collection::MAX_SEARCH_LIMIT,
            )
            .await?;
            let Some(min_id) = rows.iter().map(|r| r._id).min() else {
                break;
            };
            cursor = min_id;
            let page_len = rows.len();
            for row in rows {
                if let Some(doc_id) = row.doc_id {
                    let entry = newest.entry(doc_id).or_insert(row._id);
                    *entry = (*entry).max(row._id);
                }
            }
            if page_len < Collection::MAX_SEARCH_LIMIT {
                break;
            }
        }
        Ok(newest.into_values().collect())
    }

    /// Counts active documents whose current version is older than the
    /// threshold; persists the result for `SpaceInfo` and events it when
    /// stale documents exist.
    pub async fn stale_report(
        &self,
        now_ms: u64,
        stale_after_ms: u64,
    ) -> Result<WikiStaleReport, WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.stale_report_inner(now_ms, stale_after_ms).await })
            .await
    }

    async fn stale_report_inner(
        &self,
        now_ms: u64,
        stale_after_ms: u64,
    ) -> Result<WikiStaleReport, WikiError> {
        let threshold = now_ms.saturating_sub(stale_after_ms);
        let mut report = WikiStaleReport {
            checked_at: now_ms,
            ..Default::default()
        };
        let mut sample: Vec<Json> = Vec::new();

        let mut cursor = self.docs.max_document_id() + 1;
        loop {
            let docs: Vec<WikiDocRecord> = query_last_as(
                &self.docs,
                Filter::And(vec![
                    Box::new(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Lt(Fv::U64(cursor)),
                    ))),
                    Box::new(Filter::Field((
                        "current_version".to_string(),
                        RangeQuery::Gt(Fv::U64(0)),
                    ))),
                    Box::new(Filter::Field((
                        "status".to_string(),
                        RangeQuery::Eq(Fv::Text(DOC_STATUS_ACTIVE.to_string())),
                    ))),
                ]),
                SCAN_PAGE,
            )
            .await?;
            let Some(min_id) = docs.iter().map(|d| d._id).min() else {
                break;
            };
            cursor = min_id;
            let page_len = docs.len();
            for doc in docs {
                if doc.namespace == EVAL_NAMESPACE {
                    continue;
                }
                report.checked_docs += 1;
                if doc.updated_at < threshold {
                    report.stale_docs += 1;
                    if sample.len() < 20 {
                        sample.push(serde_json::json!({
                            "doc_id": doc._id,
                            "slug": doc.slug,
                            "updated_at": doc.updated_at,
                        }));
                    }
                }
            }
            if page_len < SCAN_PAGE {
                break;
            }
        }

        self.docs
            .save_extension(
                "wiki_stale_report".to_string(),
                serde_json::to_value(&report)
                    .map_err(|err| WikiError::Db(err.to_string()))?
                    .into(),
            )
            .await?;
        if report.stale_docs > 0 {
            self.write_event(
                EVENT_STALE_REPORT,
                None,
                None,
                "system".to_string(),
                BTreeMap::from([
                    ("stale_docs".to_string(), Json::from(report.stale_docs)),
                    ("checked_docs".to_string(), Json::from(report.checked_docs)),
                    ("sample".to_string(), Json::from(sample)),
                ]),
                now_ms,
            )
            .await?;
        }
        Ok(report)
    }

    // ─── Maintenance ────────────────────────────────────────────────────────

    // ─── Internals ──────────────────────────────────────────────────────────

    /// All ids matching `base`, paginated past the single-query cap so sets
    /// larger than 1000 (same-tag documents, thousand-version documents) are
    /// still fully enumerated.
    async fn scan_ids(
        &self,
        collection: &Arc<Collection>,
        base: Filter,
    ) -> Result<Vec<u64>, WikiError> {
        let mut out = Vec::new();
        let mut cursor = collection.max_document_id() + 1;
        loop {
            let ids = collection
                .query_last_ids(
                    Filter::And(vec![
                        Box::new(base.clone()),
                        Box::new(Filter::Field((
                            "_id".to_string(),
                            RangeQuery::Lt(Fv::U64(cursor)),
                        ))),
                    ]),
                    Some(Collection::MAX_SEARCH_LIMIT),
                )
                .await?;
            let Some(min_id) = ids.iter().copied().min() else {
                break;
            };
            let page_len = ids.len();
            out.extend(ids);
            cursor = min_id;
            if page_len < Collection::MAX_SEARCH_LIMIT {
                break;
            }
        }
        Ok(out)
    }

    async fn chunk_ids_of(&self, doc_id: u64, version_id: u64) -> Result<Vec<u64>, WikiError> {
        Ok(self
            .chunks
            .search_ids(Query {
                search: None,
                filter: Some(Filter::And(vec![
                    Box::new(Filter::Field((
                        "doc_id".to_string(),
                        RangeQuery::Eq(Fv::U64(doc_id)),
                    ))),
                    Box::new(Filter::Field((
                        "version_id".to_string(),
                        RangeQuery::Eq(Fv::U64(version_id)),
                    ))),
                ])),
                limit: Some(Collection::MAX_SEARCH_LIMIT),
            })
            .await?)
    }

    async fn all_chunk_ids_of(&self, doc_id: u64) -> Result<Vec<u64>, WikiError> {
        self.scan_ids(
            &self.chunks,
            Filter::Field(("doc_id".to_string(), RangeQuery::Eq(Fv::U64(doc_id)))),
        )
        .await
    }

    async fn version_ids_of(&self, doc_id: u64) -> Result<Vec<u64>, WikiError> {
        self.scan_ids(
            &self.versions,
            Filter::Field(("doc_id".to_string(), RangeQuery::Eq(Fv::U64(doc_id)))),
        )
        .await
    }

    async fn audit_event(
        &self,
        kind: &'static str,
        doc_id: Option<u64>,
        version_id: Option<u64>,
        actor: String,
        detail: BTreeMap<String, Json>,
        now_ms: u64,
    ) {
        let wiki = self.clone();
        let _ = self
            .owned(async move {
                wiki.write_event(kind, doc_id, version_id, actor, detail, now_ms)
                    .await
            })
            .await;
    }

    async fn write_event(
        &self,
        kind: &str,
        doc_id: Option<u64>,
        version_id: Option<u64>,
        actor: String,
        detail: BTreeMap<String, Json>,
        now_ms: u64,
    ) -> Result<u64, WikiError> {
        Ok(self
            .events
            .add_from(&WikiEventRecord {
                _id: 0,
                kind: kind.to_string(),
                doc_id,
                version_id,
                actor,
                detail,
                created_at: now_ms,
            })
            .await?)
    }

    fn cursor_or_max(
        &self,
        collection: &Arc<Collection>,
        cursor: &Option<String>,
    ) -> Result<u64, WikiError> {
        use anda_db::index::BTree;
        match BTree::from_cursor::<u64>(cursor)
            .map_err(|err| WikiError::Invalid(format!("invalid cursor: {err:?}")))?
        {
            Some(cursor) => Ok(cursor),
            None => Ok(collection.max_document_id() + 1),
        }
    }
}

fn verify_not_found() -> WikiVerifyOutput {
    WikiVerifyOutput {
        status: WikiVerifyStatus::NotFound,
        current_version: None,
        checksum: None,
        quote: None,
    }
}

fn version_info(version: &WikiVersionRecord, id: u64) -> WikiVersionInfo {
    WikiVersionInfo {
        id,
        doc_id: version.doc_id,
        parent_version: version.parent_version,
        checksum: version.checksum.clone(),
        size: version.size,
        author: version.author.clone(),
        message: version.message.clone(),
        created_at: version.created_at,
    }
}

/// ACL prefilter clause: unlabeled content plus the granted labels. Runs
/// inside the same AndaDB query as retrieval/listing.
fn acl_filter(labels: &[String]) -> Filter {
    let mut allowed: Vec<Fv> = Vec::with_capacity(labels.len() + 1);
    allowed.push(Fv::Text(String::new()));
    allowed.extend(
        labels
            .iter()
            .take(MAX_INCLUDE_KEYS - 1)
            .cloned()
            .map(Fv::Text),
    );
    Filter::Field(("acl_label".to_string(), RangeQuery::Include(allowed)))
}

/// Pages are ascending "newest N below cursor"; the next cursor is the
/// smallest id in a full page (same convention as conversation listing).
fn page_cursor<T>(rows: &[T], limit: usize, id_of: impl Fn(&T) -> u64) -> Option<String> {
    use anda_db::index::BTree;
    if rows.len() >= limit {
        rows.iter()
            .map(&id_of)
            .min()
            .and_then(|id| BTree::to_cursor(&id))
    } else {
        None
    }
}

async fn query_last_as<T>(
    collection: &Collection,
    filter: Filter,
    limit: usize,
) -> Result<Vec<T>, WikiError>
where
    T: serde::de::DeserializeOwned,
{
    let ids = collection.query_last_ids(filter, Some(limit)).await?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        rows.push(collection.get_as(id).await?);
    }
    Ok(rows)
}

async fn init_wiki_docs(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["namespace"]).await?;
    collection.create_btree_index_nx(&["slug"]).await?;
    collection.create_btree_index_nx(&["status"]).await?;
    collection.create_btree_index_nx(&["tags"]).await?;
    collection
        .create_btree_index_nx(&["current_version"])
        .await?;
    collection.create_btree_index_nx(&["acl_label"]).await?;
    collection
        .create_btree_index_nx(&["digest_pending"])
        .await?;
    collection.create_bm25_index_nx(&["title", "tags"]).await?;
    Ok(())
}

async fn init_wiki_versions(collection: &mut Collection) -> Result<(), DBError> {
    collection.create_btree_index_nx(&["doc_id"]).await?;
    collection.create_btree_index_nx(&["checksum"]).await?;
    Ok(())
}

async fn init_wiki_chunks(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["doc_id"]).await?;
    collection.create_btree_index_nx(&["version_id"]).await?;
    collection.create_btree_index_nx(&["namespace"]).await?;
    collection.create_btree_index_nx(&["current"]).await?;
    collection.create_btree_index_nx(&["acl_label"]).await?;
    collection
        .create_bm25_index_nx(&["title", "heading_path", "text"])
        .await?;
    Ok(())
}

async fn init_wiki_events(collection: &mut Collection) -> Result<(), DBError> {
    collection.create_btree_index_nx(&["kind"]).await?;
    collection.create_btree_index_nx(&["doc_id"]).await?;
    collection.create_btree_index_nx(&["actor"]).await?;
    collection.create_btree_index_nx(&["created_at"]).await?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod exchange_tests;

#[cfg(test)]
mod access_tests;
