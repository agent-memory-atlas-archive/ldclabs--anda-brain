//! AndaDB records for the wiki subsystem.
//!
//! `wiki_docs`, `wiki_versions` and `wiki_events` are sources of truth;
//! `wiki_chunks` is a derived retrieval plane that can always be rebuilt
//! from version content.

use anda_db::schema::{AndaDBSchema, Json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DOC_STATUS_ACTIVE: &str = "active";
pub const DOC_STATUS_ARCHIVED: &str = "archived";

pub const EVENT_DOC_CREATED: &str = "DocCreated";
pub const EVENT_VERSION_COMMITTED: &str = "VersionCommitted";
pub const EVENT_DOC_ARCHIVED: &str = "DocArchived";
pub const EVENT_DOC_RESTORED: &str = "DocRestored";
pub const EVENT_ORPHAN_SWEPT: &str = "OrphanSwept";
pub const EVENT_IMPORT_COMPLETED: &str = "ImportCompleted";
pub const EVENT_EXPORT_COMPLETED: &str = "ExportCompleted";
pub const EVENT_DIGEST_EXTRACTED: &str = "DigestExtracted";
pub const EVENT_DIGEST_FAILED: &str = "DigestFailed";
pub const EVENT_WIKI_QUERIED: &str = "WikiQueried";
pub const EVENT_WIKI_READ: &str = "WikiRead";
pub const EVENT_STALE_REPORT: &str = "StaleReport";
pub const EVENT_EVENTS_PRUNED: &str = "EventsPruned";

/// Document registry row. `current_version == 0` marks a document that is
/// still initializing (created but never activated): invisible to reads and
/// reclaimed by the orphan sweep if its commit crashed.
#[derive(Debug, Clone, Serialize, Deserialize, AndaDBSchema)]
pub struct WikiDocRecord {
    pub _id: u64,
    pub namespace: String,
    pub slug: String,
    pub title: String,
    pub status: String,
    pub current_version: u64,
    /// Changes with content, ACL and archive/restore. Host-only fencing token.
    pub generation: u64,
    /// 1 until the current generation has been reconciled by WikiDigest.
    /// Published atomically with every document state change.
    pub digest_pending: u64,
    pub current_checksum: String,
    pub tags: Vec<String>,
    /// ACL label; empty = readable by any space reader. Restricted tokens
    /// see only unlabeled documents plus their granted labels (PRD §8.2).
    pub acl_label: String,
    pub source_uri: Option<String>,
    pub metadata: BTreeMap<String, Json>,
    pub created_by: String,
    pub updated_by: String,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Immutable commit row. Never updated or removed once its document points
/// at it; orphan rows from crashed commits are the only exception.
#[derive(Debug, Clone, Serialize, Deserialize, AndaDBSchema)]
pub struct WikiVersionRecord {
    pub _id: u64,
    pub doc_id: u64,
    pub parent_version: Option<u64>,
    pub checksum: String,
    pub content: String,
    pub size: u64,
    pub author: String,
    pub message: Option<String>,
    pub created_at: u64,
}

/// Retrieval-plane row. `current` and the copied ACL label are prefilters;
/// the document snapshot authorizes the version actually returned. `text` is the exact
/// `content[byte_start..byte_end]` slice of its version.
#[derive(Debug, Clone, Serialize, Deserialize, AndaDBSchema)]
pub struct WikiChunkRecord {
    pub _id: u64,
    pub doc_id: u64,
    pub version_id: u64,
    pub namespace: String,
    pub current: u64,
    pub title: String,
    pub heading_path: Vec<String>,
    pub anchor: String,
    pub ordinal: u64,
    pub text: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub checksum: String,
    pub chunker_version: u64,
    /// Denormalized from the document so the ACL prefilter runs inside the
    /// same AndaDB query as retrieval (empty = unlabeled).
    pub acl_label: String,
}

/// Audit row for writes and background tasks, plus optional external reads.
/// Retention preserves the latest digest ledger for each document.
#[derive(Debug, Clone, Serialize, Deserialize, AndaDBSchema)]
pub struct WikiEventRecord {
    pub _id: u64,
    pub kind: String,
    pub doc_id: Option<u64>,
    pub version_id: Option<u64>,
    pub actor: String,
    pub detail: BTreeMap<String, Json>,
    pub created_at: u64,
}
