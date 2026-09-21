//! Document publication, immutable history and recovery.

use super::*;

impl WikiService {
    /// The single write primitive: an immutable commit (see PRD §4.1).
    ///
    /// Ordering is crash-safe: version row → inactive chunks → doc flip (the
    /// activation point) → chunk activation → superseded-chunk removal →
    /// event. A crash at any step leaves either the old version fully
    /// visible or the new one; leftovers are invisible and reclaimed by
    /// [`WikiService::orphan_sweep`].
    pub async fn commit(
        &self,
        actor: String,
        input: WikiCommitInput,
        now_ms: u64,
    ) -> Result<WikiCommitOutput, WikiError> {
        let prepared = prepare_commit(input)?;
        let wiki = self.clone();
        self.owned(async move {
            let _guard = wiki.write_lock.lock().await;
            wiki.commit_prepared(actor, prepared, now_ms).await
        })
        .await
    }

    /// Commit core; the caller MUST hold [`WikiService::write_lock`]. Split
    /// from [`WikiService::commit`] so OKF import can run "slug lookup +
    /// commit" atomically under one lock acquisition.
    pub(super) async fn commit_prepared(
        &self,
        actor: String,
        prepared: PreparedCommit,
        now_ms: u64,
    ) -> Result<WikiCommitOutput, WikiError> {
        let PreparedCommit {
            input,
            content,
            checksum,
            forced_splits,
            capped,
            chunks: mut chunk_rows,
        } = prepared;

        let existing = match input.doc_id {
            Some(id) => {
                let doc = self.doc_record(id).await?;
                if doc.status == DOC_STATUS_ARCHIVED {
                    return Err(WikiError::Invalid(format!(
                        "document {id} is archived; restore it before committing"
                    )));
                }
                let parent = input.parent_version.ok_or_else(|| {
                    WikiError::Invalid("parent_version is required when updating".into())
                })?;
                if parent != doc.current_version {
                    return Err(WikiError::Conflict {
                        current_version: doc.current_version,
                        current_checksum: doc.current_checksum.clone(),
                        updated_by: doc.updated_by.clone(),
                        updated_at: doc.updated_at,
                    });
                }
                Some(doc)
            }
            None => None,
        };

        // Idempotent short-circuit: nothing changed, nothing written.
        if let Some(doc) = &existing
            && doc.current_checksum == checksum
            && doc.title == input.title
            && input.tags.as_ref().is_none_or(|tags| *tags == doc.tags)
            && input
                .acl_label
                .as_ref()
                .is_none_or(|label| label.trim() == doc.acl_label)
            && input
                .namespace
                .as_ref()
                .is_none_or(|ns| *ns == doc.namespace)
            // Compare in slug space: replaying a commit whose raw slug is
            // non-canonical ("Setup Steps" vs "setup-steps") stays a no-op.
            && input
                .slug
                .as_ref()
                .is_none_or(|s| slugify_path(s) == doc.slug)
            && input.source_uri.as_ref().is_none_or(|s| {
                // `Some("")` means "cleared": idempotent against a document
                // that has no source_uri.
                if s.is_empty() {
                    doc.source_uri.is_none()
                } else {
                    Some(s) == doc.source_uri.as_ref()
                }
            })
            && input.metadata.as_ref().is_none_or(|m| *m == doc.metadata)
        {
            let version = self.version_record(doc.current_version).await?;
            let chunks = self.chunk_ids_of(doc._id, doc.current_version).await?.len();
            return Ok(WikiCommitOutput {
                doc: doc.clone().into(),
                version: version_info(&version, doc.current_version),
                chunks,
                created: false,
                idempotent: true,
            });
        }

        // None keeps stored tags on update; empty documents start with none.
        let tags = match (&input.tags, &existing) {
            (Some(tags), _) => tags.clone(),
            (None, Some(doc)) => doc.tags.clone(),
            (None, None) => Vec::new(),
        };
        // None keeps the stored label; on create it inherits the namespace
        // default. Some("") clears explicitly.
        let acl_label = match (&input.acl_label, &existing) {
            (Some(label), _) => label.trim().to_string(),
            (None, Some(doc)) => doc.acl_label.clone(),
            (None, None) => {
                self.acl_default_for(input.namespace.as_deref().unwrap_or(DEFAULT_NAMESPACE))
            }
        };

        let (doc_id, created, namespace, slug, prev_version) = match &existing {
            Some(doc) => {
                let namespace = input
                    .namespace
                    .clone()
                    .unwrap_or_else(|| doc.namespace.clone());
                let want_slug = input.slug.clone().unwrap_or_else(|| doc.slug.clone());
                let slug = if want_slug != doc.slug || namespace != doc.namespace {
                    self.unique_slug(&namespace, &want_slug, Some(doc._id), now_ms)
                        .await?
                } else {
                    doc.slug.clone()
                };
                (doc._id, false, namespace, slug, doc.current_version)
            }
            None => {
                let namespace = input
                    .namespace
                    .clone()
                    .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
                let base = input.slug.clone().unwrap_or_else(|| slugify(&input.title));
                let slug = self.unique_slug(&namespace, &base, None, now_ms).await?;
                let doc = WikiDocRecord {
                    generation: 1,
                    digest_pending: 0,
                    _id: 0,
                    namespace: namespace.clone(),
                    slug: slug.clone(),
                    title: input.title.clone(),
                    status: DOC_STATUS_ACTIVE.to_string(),
                    current_version: 0,
                    current_checksum: String::new(),
                    tags: tags.clone(),
                    acl_label: acl_label.clone(),
                    source_uri: input.source_uri.clone(),
                    metadata: input.metadata.clone().unwrap_or_default(),
                    created_by: actor.clone(),
                    updated_by: actor.clone(),
                    created_at: now_ms,
                    updated_at: now_ms,
                };
                let id = self.docs.add_from(&doc).await?;
                (id, true, namespace, slug, 0)
            }
        };

        // 1) Immutable version row.
        let version = WikiVersionRecord {
            _id: 0,
            doc_id,
            parent_version: (!created).then_some(prev_version),
            checksum: checksum.clone(),
            content: content.clone(),
            size: content.len() as u64,
            author: actor.clone(),
            message: input.message.clone(),
            created_at: now_ms,
        };
        let version_id = self.versions.add_from(&version).await?;

        // 2) Chunks, inactive until the doc flips. The rows were prebuilt
        //    outside the lock (pure CPU); only the per-document fields are
        //    patched in here.
        let mut chunk_ids = Vec::with_capacity(chunk_rows.len());
        for record in &mut chunk_rows {
            record.doc_id = doc_id;
            record.version_id = version_id;
            record.namespace = namespace.clone();
            record.acl_label = acl_label.clone();
            chunk_ids.push(self.chunks.add_from(record).await?);
        }

        // 3) Activation point: one doc update makes the commit effective.
        let mut fields = BTreeMap::from([
            ("namespace".to_string(), Fv::Text(namespace.clone())),
            ("slug".to_string(), Fv::Text(slug)),
            ("title".to_string(), Fv::Text(input.title.clone())),
            (
                "status".to_string(),
                Fv::Text(DOC_STATUS_ACTIVE.to_string()),
            ),
            ("current_version".to_string(), Fv::U64(version_id)),
            (
                "generation".into(),
                Fv::U64(existing.as_ref().map_or(1, |doc| doc.generation + 1)),
            ),
            ("digest_pending".into(), Fv::U64(1)),
            ("current_checksum".to_string(), Fv::Text(checksum.clone())),
            (
                "tags".to_string(),
                Fv::Array(tags.iter().cloned().map(Fv::Text).collect()),
            ),
            ("acl_label".to_string(), Fv::Text(acl_label.clone())),
            ("updated_by".to_string(), Fv::Text(actor.clone())),
            ("updated_at".to_string(), Fv::U64(now_ms)),
        ]);
        // None keeps the stored source_uri on update; empty clears it — a
        // deleted `resource:` key must propagate on OKF re-import instead of
        // being resurrected by the next export (mirroring tags).
        match input.source_uri.as_deref() {
            None => {}
            Some("") => {
                fields.insert("source_uri".to_string(), Fv::Null);
            }
            Some(uri) => {
                fields.insert("source_uri".to_string(), Fv::Text(uri.to_string()));
            }
        }
        if let Some(metadata) = &input.metadata {
            fields.insert("metadata".to_string(), Fv::from(metadata.clone()));
        }
        self.docs.update(doc_id, fields).await?;

        // 4) Activate retrieval rows. Reads authorize against the published
        //    document version, so superseded rows never grant visibility.
        for id in &chunk_ids {
            self.chunks
                .update(*id, BTreeMap::from([("current".to_string(), Fv::U64(1))]))
                .await?;
        }

        // 5) Remove the superseded chunk set.
        if !created {
            for id in self.chunk_ids_of(doc_id, prev_version).await? {
                self.chunks.remove(id).await?;
            }
        }

        // 6) Audit event.
        let mut detail = BTreeMap::from([
            ("chunks".to_string(), Json::from(chunk_rows.len() as u64)),
            ("checksum".to_string(), Json::from(checksum)),
            ("size".to_string(), Json::from(content.len() as u64)),
        ]);
        if forced_splits > 0 {
            detail.insert(
                "forced_splits".to_string(),
                Json::from(forced_splits as u64),
            );
        }
        if capped {
            detail.insert("chunks_capped".to_string(), Json::from(true));
        }
        if let Some(message) = &input.message {
            detail.insert("message".to_string(), Json::from(message.clone()));
        }
        self.write_event(
            if created {
                EVENT_DOC_CREATED
            } else {
                EVENT_VERSION_COMMITTED
            },
            Some(doc_id),
            Some(version_id),
            actor,
            detail,
            now_ms,
        )
        .await?;

        let doc = self.doc_record(doc_id).await?;
        let stored = self.version_record(version_id).await?;
        Ok(WikiCommitOutput {
            doc: doc.into(),
            version: version_info(&stored, version_id),
            chunks: chunk_rows.len(),
            created,
            idempotent: false,
        })
    }

    pub async fn archive(
        &self,
        actor: String,
        doc_id: u64,
        now_ms: u64,
    ) -> Result<WikiDocInfo, WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.archive_inner(actor, doc_id, now_ms).await })
            .await
    }

    pub(super) async fn archive_inner(
        &self,
        actor: String,
        doc_id: u64,
        now_ms: u64,
    ) -> Result<WikiDocInfo, WikiError> {
        let _guard = self.write_lock.lock().await;
        let doc = self.doc_record(doc_id).await?;
        if doc.status == DOC_STATUS_ARCHIVED {
            return Err(WikiError::Invalid(format!(
                "document {doc_id} is already archived"
            )));
        }
        self.docs
            .update(
                doc_id,
                BTreeMap::from([
                    (
                        "status".to_string(),
                        Fv::Text(DOC_STATUS_ARCHIVED.to_string()),
                    ),
                    ("generation".into(), Fv::U64(doc.generation + 1)),
                    ("digest_pending".into(), Fv::U64(1)),
                    ("updated_by".to_string(), Fv::Text(actor.clone())),
                    ("updated_at".to_string(), Fv::U64(now_ms)),
                ]),
            )
            .await?;
        self.set_chunks_current(doc_id, doc.current_version, false)
            .await?;
        self.write_event(
            EVENT_DOC_ARCHIVED,
            Some(doc_id),
            Some(doc.current_version),
            actor,
            BTreeMap::new(),
            now_ms,
        )
        .await?;
        Ok(self.doc_record(doc_id).await?.into())
    }

    pub async fn restore(
        &self,
        actor: String,
        doc_id: u64,
        now_ms: u64,
    ) -> Result<WikiDocInfo, WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.restore_inner(actor, doc_id, now_ms).await })
            .await
    }

    pub(super) async fn restore_inner(
        &self,
        actor: String,
        doc_id: u64,
        now_ms: u64,
    ) -> Result<WikiDocInfo, WikiError> {
        let _guard = self.write_lock.lock().await;
        let doc = self.doc_record(doc_id).await?;
        if doc.status != DOC_STATUS_ARCHIVED {
            return Err(WikiError::Invalid(format!(
                "document {doc_id} is not archived"
            )));
        }
        self.docs
            .update(
                doc_id,
                BTreeMap::from([
                    (
                        "status".to_string(),
                        Fv::Text(DOC_STATUS_ACTIVE.to_string()),
                    ),
                    ("generation".into(), Fv::U64(doc.generation + 1)),
                    ("digest_pending".into(), Fv::U64(1)),
                    ("updated_by".to_string(), Fv::Text(actor.clone())),
                    ("updated_at".to_string(), Fv::U64(now_ms)),
                ]),
            )
            .await?;
        self.set_chunks_current(doc_id, doc.current_version, true)
            .await?;
        self.write_event(
            EVENT_DOC_RESTORED,
            Some(doc_id),
            Some(doc.current_version),
            actor,
            BTreeMap::new(),
            now_ms,
        )
        .await?;
        Ok(self.doc_record(doc_id).await?.into())
    }

    /// Reclaims commit-crash leftovers and repairs chunk visibility. Runs
    /// under the write lock so in-flight commits are never mistaken for
    /// orphans. Safe to run at any time; called on space startup.
    pub async fn orphan_sweep(&self, now_ms: u64) -> Result<WikiSweepReport, WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.orphan_sweep_inner(now_ms).await })
            .await
    }

    pub(super) async fn orphan_sweep_inner(
        &self,
        now_ms: u64,
    ) -> Result<WikiSweepReport, WikiError> {
        let _guard = self.write_lock.lock().await;
        let mut report = WikiSweepReport::default();

        let mut cursor = self.docs.max_document_id() + 1;
        loop {
            let docs: Vec<WikiDocRecord> = query_last_as(
                &self.docs,
                Filter::Field(("_id".to_string(), RangeQuery::Lt(Fv::U64(cursor)))),
                SCAN_PAGE,
            )
            .await?;
            let Some(min_id) = docs.iter().map(|d| d._id).min() else {
                break;
            };
            cursor = min_id;
            let page_len = docs.len();
            for doc in docs {
                self.sweep_doc(doc, now_ms, &mut report).await?;
            }
            if page_len < SCAN_PAGE {
                break;
            }
        }

        if !report.is_empty() {
            self.write_event(
                EVENT_ORPHAN_SWEPT,
                None,
                None,
                "system".to_string(),
                BTreeMap::from([
                    (
                        "docs_removed".to_string(),
                        Json::from(report.docs_removed as u64),
                    ),
                    (
                        "versions_removed".to_string(),
                        Json::from(report.versions_removed as u64),
                    ),
                    (
                        "chunks_removed".to_string(),
                        Json::from(report.chunks_removed as u64),
                    ),
                    (
                        "chunks_repaired".to_string(),
                        Json::from(report.chunks_repaired as u64),
                    ),
                ]),
                now_ms,
            )
            .await?;
        }
        Ok(report)
    }

    pub(super) async fn sweep_doc(
        &self,
        doc: WikiDocRecord,
        now_ms: u64,
        report: &mut WikiSweepReport,
    ) -> Result<(), WikiError> {
        // A create that never activated: reclaim the whole document once the
        // in-flight window has clearly passed.
        if doc.current_version == 0 {
            if now_ms.saturating_sub(doc.created_at) > SENTINEL_TTL_MS {
                for version_id in self.version_ids_of(doc._id).await? {
                    self.versions.remove(version_id).await?;
                    report.versions_removed += 1;
                }
                for chunk_id in self.all_chunk_ids_of(doc._id).await? {
                    self.chunks.remove(chunk_id).await?;
                    report.chunks_removed += 1;
                }
                self.docs.remove(doc._id).await?;
                report.docs_removed += 1;
            }
            return Ok(());
        }

        let published = self.published_version_ids(&doc).await?;
        let orphan_versions: Vec<u64> = self
            .version_ids_of(doc._id)
            .await?
            .into_iter()
            .filter(|id| !published.contains(id))
            .collect();
        for version_id in orphan_versions {
            for chunk_id in self.chunk_ids_of(doc._id, version_id).await? {
                self.chunks.remove(chunk_id).await?;
                report.chunks_removed += 1;
            }
            self.versions.remove(version_id).await?;
            report.versions_removed += 1;
        }

        // Reconcile chunk visibility with the doc row. `scan_ids` paginates
        // past the single-query cap: after a crash between activation and
        // cleanup a document briefly owns two full chunk sets, which can
        // exceed one search page.
        let keep: std::collections::BTreeSet<u64> = self
            .chunk_ids_of(doc._id, doc.current_version)
            .await?
            .into_iter()
            .collect();
        for chunk_id in self.all_chunk_ids_of(doc._id).await? {
            if !keep.contains(&chunk_id) {
                self.chunks.remove(chunk_id).await?;
                report.chunks_removed += 1;
            }
        }
        let want_current: u64 = (doc.status == DOC_STATUS_ACTIVE) as u64;
        let mismatched = self
            .scan_ids(
                &self.chunks,
                Filter::And(vec![
                    Box::new(Filter::Field((
                        "doc_id".to_string(),
                        RangeQuery::Eq(Fv::U64(doc._id)),
                    ))),
                    Box::new(Filter::Field((
                        "version_id".to_string(),
                        RangeQuery::Eq(Fv::U64(doc.current_version)),
                    ))),
                    Box::new(Filter::Field((
                        "current".to_string(),
                        RangeQuery::Eq(Fv::U64(1 - want_current)),
                    ))),
                ]),
            )
            .await?;
        for chunk_id in mismatched {
            self.chunks
                .update(
                    chunk_id,
                    BTreeMap::from([("current".to_string(), Fv::U64(want_current))]),
                )
                .await?;
            report.chunks_repaired += 1;
        }
        Ok(())
    }

    /// Loads a document, treating initializing sentinels as absent.
    pub(super) async fn doc_record(&self, doc_id: u64) -> Result<WikiDocRecord, WikiError> {
        let doc: WikiDocRecord = self.docs.get_as(doc_id).await.map_err(WikiError::from)?;
        if doc.current_version == 0 {
            return Err(WikiError::NotFound(format!("document {doc_id} not found")));
        }
        Ok(doc)
    }

    pub(super) async fn version_record(
        &self,
        version_id: u64,
    ) -> Result<WikiVersionRecord, WikiError> {
        self.versions
            .get_as(version_id)
            .await
            .map_err(WikiError::from)
    }

    /// Version membership is defined by published parents, never by numeric id.
    pub(super) async fn chain_version(
        &self,
        doc_id: u64,
        id: u64,
    ) -> Result<WikiVersionRecord, WikiError> {
        let version = self.version_record(id).await?;
        if version.doc_id != doc_id
            || version
                .parent_version
                .is_some_and(|parent| parent == 0 || parent >= id)
        {
            return Err(WikiError::Db(format!(
                "invalid version chain for document {doc_id} at {id}"
            )));
        }
        Ok(version)
    }

    pub(super) async fn published_version(
        &self,
        doc: &WikiDocRecord,
        version_id: u64,
    ) -> Result<WikiVersionRecord, WikiError> {
        let mut next = Some(doc.current_version);
        while let Some(id) = next {
            if id < version_id {
                break;
            }
            let version = self.chain_version(doc._id, id).await?;
            if id == version_id {
                return Ok(version);
            }
            next = version.parent_version;
        }
        Err(WikiError::NotFound(format!(
            "version {version_id} is not published for document {}",
            doc._id
        )))
    }

    pub(super) async fn published_version_ids(
        &self,
        doc: &WikiDocRecord,
    ) -> Result<std::collections::BTreeSet<u64>, WikiError> {
        let mut ids = std::collections::BTreeSet::new();
        let mut next = Some(doc.current_version);
        while let Some(id) = next {
            ids.insert(id);
            next = self.chain_version(doc._id, id).await?.parent_version;
        }
        Ok(ids)
    }

    pub(super) async fn document_versions(
        &self,
        doc: &WikiDocRecord,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<WikiVersionListOutput, WikiError> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let cursor = self.cursor_or_max(&self.versions, &cursor)?;
        let mut rows = Vec::new();
        let mut next = Some(doc.current_version);
        while let Some(id) = next {
            let version = self.chain_version(doc._id, id).await?;
            next = version.parent_version;
            if id < cursor {
                rows.push(version);
                if rows.len() == limit {
                    break;
                }
            }
        }
        rows.reverse();
        let next_cursor = page_cursor(&rows, limit, |v| v._id);
        Ok(WikiVersionListOutput {
            versions: rows.iter().map(|v| version_info(v, v._id)).collect(),
            next_cursor,
        })
    }

    pub(super) async fn find_doc_id_by_slug(
        &self,
        namespace: &str,
        slug: &str,
    ) -> Result<Option<u64>, WikiError> {
        let rows: Vec<WikiDocRecord> = self
            .docs
            .search_as(Query {
                search: None,
                filter: Some(Filter::And(vec![
                    Box::new(Filter::Field((
                        "namespace".to_string(),
                        RangeQuery::Eq(Fv::Text(namespace.to_string())),
                    ))),
                    Box::new(Filter::Field((
                        "slug".to_string(),
                        RangeQuery::Eq(Fv::Text(slug.to_string())),
                    ))),
                ])),
                limit: Some(1),
            })
            .await?;
        Ok(rows.first().map(|doc| doc._id))
    }

    /// Resolves a unique slug within a namespace by suffixing `-2`, `-3`, …
    /// on collision. Never merges two documents (the v1 failure mode).
    pub(super) async fn unique_slug(
        &self,
        namespace: &str,
        base: &str,
        exclude: Option<u64>,
        now_ms: u64,
    ) -> Result<String, WikiError> {
        // Path form preserves `/` hierarchy (OKF concept ids); titles are
        // already flat after slugify, so plain slugs pass through unchanged.
        let base = slugify_path(base);
        // "index"/"log" are reserved OKF bundle files (PRD §9): a slug ending
        // in either would collide with generated files on export and be
        // silently skipped on replay.
        let reserved = |slug: &str| {
            slug.rsplit('/')
                .next()
                .is_some_and(|last| last == "index" || last == "log")
        };
        for attempt in 0..100u32 {
            let candidate = if attempt == 0 {
                base.clone()
            } else {
                format!("{base}-{}", attempt + 1)
            };
            if reserved(&candidate) {
                continue;
            }
            match self.find_doc_id_by_slug(namespace, &candidate).await? {
                None => return Ok(candidate),
                Some(id) if Some(id) == exclude => return Ok(candidate),
                Some(_) => {}
            }
        }
        Ok(format!("{base}-{now_ms}"))
    }

    pub(super) async fn set_chunks_current(
        &self,
        doc_id: u64,
        version_id: u64,
        current: bool,
    ) -> Result<(), WikiError> {
        for id in self.chunk_ids_of(doc_id, version_id).await? {
            self.chunks
                .update(
                    id,
                    BTreeMap::from([("current".to_string(), Fv::U64(current as u64))]),
                )
                .await?;
        }
        Ok(())
    }
}

/// Pure-CPU commit preparation: normalization, validation, checksums, the
/// chunk plan and the chunk rows themselves. Computed before the write lock
/// is taken so the lock hold covers only storage writes (a 1 MiB document
/// chunks and hashes outside it). The per-document fields of `chunks`
/// (doc/version ids, namespace, ACL label) are patched in under the lock.
pub(super) struct PreparedCommit {
    pub(super) input: WikiCommitInput,
    pub(super) content: String,
    pub(super) checksum: String,
    pub(super) forced_splits: usize,
    pub(super) capped: bool,
    pub(super) chunks: Vec<WikiChunkRecord>,
}

pub(super) fn prepare_commit(mut input: WikiCommitInput) -> Result<PreparedCommit, WikiError> {
    input.normalize();
    if input.title.is_empty() {
        return Err(WikiError::Invalid(
            "title is required (or provide a markdown heading)".into(),
        ));
    }
    input.validate()?;
    let content = normalize_content(&input.content);
    if content.is_empty() {
        return Err(WikiError::Invalid("content cannot be empty".into()));
    }
    if content.len() > MAX_DOC_BYTES {
        return Err(WikiError::TooLarge {
            size: content.len(),
            max: MAX_DOC_BYTES,
        });
    }
    let checksum = checksum_for([content.as_bytes()]);
    let plan = chunk_markdown(&content);
    let outline = outline::outline(&content);
    let chunks = plan
        .drafts
        .iter()
        .enumerate()
        .map(|(idx, draft)| {
            let text = &content[draft.byte_start..draft.byte_end];
            WikiChunkRecord {
                _id: 0,
                doc_id: 0,
                version_id: 0,
                namespace: String::new(),
                current: 0,
                title: input.title.clone(),
                heading_path: draft.heading_path.clone(),
                anchor: outline::anchor_covering(&outline, draft.byte_start, draft.byte_end),
                ordinal: idx as u64,
                text: text.to_string(),
                byte_start: draft.byte_start as u64,
                byte_end: draft.byte_end as u64,
                checksum: chunk_checksum(&checksum, draft.byte_start, draft.byte_end, text),
                chunker_version: CHUNKER_VERSION as u64,
                acl_label: String::new(),
            }
        })
        .collect();
    Ok(PreparedCommit {
        input,
        content,
        checksum,
        forced_splits: plan.forced_splits,
        capped: plan.capped,
        chunks,
    })
}
