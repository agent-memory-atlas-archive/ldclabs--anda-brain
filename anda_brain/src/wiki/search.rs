//! BM25 candidates, document authorization and citation expansion.

use super::*;
use futures::{StreamExt, TryStreamExt};

impl WikiService {
    /// One-call BM25 retrieval over the chunk plane. Visibility (current
    /// version, namespace, archive state) is entirely in the filter, which
    /// AndaDB applies in the same query while preserving relevance order.
    /// Unscoped, unevented search over the unrestricted view; production
    /// paths go through `search_scoped` (HTTP/MCP) and `search_view`
    /// (agent tool), so this stays test-only.
    #[cfg(test)]
    pub(super) async fn search(
        &self,
        input: WikiSearchInput,
    ) -> Result<WikiSearchOutput, WikiError> {
        self.bump_query_count();
        self.search_inner(input, None).await
    }

    /// Retrieval for engine tools (RecallAgent): like [`WikiService::search`]
    /// but honoring an optional label view. Public spaces pass
    /// `Some(&[])` because their recall endpoint is world-reachable and must
    /// not surface labeled content (PRD §8.2); private spaces pass `None`.
    /// Agent reads are not evented — the recall conversation log covers them
    /// (PRD §3.4).
    pub async fn search_view(
        &self,
        input: WikiSearchInput,
        view: Option<&[String]>,
    ) -> Result<WikiSearchOutput, WikiError> {
        self.bump_query_count();
        self.search_inner(input, view).await
    }

    /// Label-scoped retrieval for external (HTTP/MCP) callers: the ACL
    /// prefilter is part of the same AndaDB query, and the read is evented
    /// when `audit_reads` is enabled. Note the prefilter operates on the
    /// BM25 candidate pool (top_k×10, capped at 4096 by AndaDB), so a
    /// heavily restricted caller querying a hot term can see fewer than
    /// `top_k` hits even when deeper matches exist — never more.
    pub async fn search_scoped(
        &self,
        access: &WikiAccess,
        input: WikiSearchInput,
        now_ms: u64,
    ) -> Result<WikiSearchOutput, WikiError> {
        self.bump_query_count();
        let query = input.query.clone();
        let output = self.search_inner(input, access.labels.as_deref()).await?;
        if self.audit_reads() {
            // Read-audit failures never fail the read.
            let _ = self
                .audit_event(
                    EVENT_WIKI_QUERIED,
                    None,
                    None,
                    access.actor.clone(),
                    BTreeMap::from([
                        ("query".to_string(), Json::from(query)),
                        ("hits".to_string(), Json::from(output.hits.len() as u64)),
                        (
                            "restricted".to_string(),
                            Json::from(access.labels.is_some()),
                        ),
                    ]),
                    now_ms,
                )
                .await;
        }
        Ok(output)
    }

    pub(super) async fn search_inner(
        &self,
        mut input: WikiSearchInput,
        labels: Option<&[String]>,
    ) -> Result<WikiSearchOutput, WikiError> {
        input.normalize();
        if input.query.is_empty() {
            return Err(WikiError::Invalid("query cannot be empty".into()));
        }
        // Oversized filter lists narrow silently: Include truncation is
        // fail-closed (results can only shrink, never widen).
        input.namespaces.truncate(MAX_INCLUDE_KEYS);
        input.doc_ids.truncate(MAX_INCLUDE_KEYS);
        let top_k = input.top_k.unwrap_or(8).clamp(1, 50);

        let mut doc_ids = input.doc_ids.clone();
        if !input.tags.is_empty() {
            let mut tagged = self.doc_ids_by_tags(&input.tags).await?;
            if tagged.len() > MAX_INCLUDE_KEYS {
                // Fail-closed: results narrow, never widen. Loud so a space
                // with >4096 same-tag documents shows up in the logs.
                log::warn!(
                    target: "brain",
                    space_id = self.space_id,
                    tagged = tagged.len();
                    "wiki tag filter truncated to {MAX_INCLUDE_KEYS} documents"
                );
                tagged.truncate(MAX_INCLUDE_KEYS);
            }
            doc_ids = if doc_ids.is_empty() {
                tagged
            } else {
                doc_ids.retain(|id| tagged.contains(id));
                doc_ids
            };
            if doc_ids.is_empty() {
                return Ok(WikiSearchOutput::default());
            }
        }

        let mut filters: Vec<Box<Filter>> = vec![Box::new(Filter::Field((
            "current".to_string(),
            RangeQuery::Eq(Fv::U64(1)),
        )))];
        // Without an explicit namespace list, the retrieval-eval corpus must
        // stay out of results (it lives in the wiki but is not user
        // content). That exclusion is applied AFTER the query: a `Filter::
        // Not` would make anda_db walk the collection's entire id index on
        // every default search (the BM25 candidate set only gates
        // membership), an O(collection) cost on the hottest path.
        let exclude_eval = input.namespaces.is_empty();
        if !input.namespaces.is_empty() {
            filters.push(Box::new(Filter::Field((
                "namespace".to_string(),
                RangeQuery::Include(input.namespaces.iter().cloned().map(Fv::Text).collect()),
            ))));
        }
        if !doc_ids.is_empty() {
            filters.push(Box::new(Filter::Field((
                "doc_id".to_string(),
                RangeQuery::Include(doc_ids.iter().copied().map(Fv::U64).collect()),
            ))));
        }
        if let Some(labels) = labels {
            filters.push(Box::new(acl_filter(labels)));
        }

        let fetch = match input.mode {
            WikiSearchMode::Chunks => top_k,
            WikiSearchMode::Docs => (top_k * 8).clamp(top_k, 200),
        };
        let mut rows: Vec<WikiChunkRecord> = self
            .chunks
            .search_as(Query {
                search: Some(Search {
                    text: Some(input.query.clone()),
                    ..Default::default()
                }),
                filter: Some(Filter::And(filters)),
                limit: Some(fetch),
            })
            .await?;
        // The document snapshot, not denormalized chunk fields, authorizes each hit.
        let access = WikiAccess {
            actor: String::new(),
            labels: labels.map(<[String]>::to_vec),
        };
        let doc_ids: std::collections::BTreeSet<_> = rows.iter().map(|row| row.doc_id).collect();
        let visible: BTreeMap<_, _> = futures::stream::iter(doc_ids)
            .map(|id| async move {
                let doc = match self.doc_record(id).await {
                    Ok(doc) => Some(doc),
                    Err(WikiError::NotFound(_)) => None,
                    Err(error) => return Err(error),
                };
                Ok::<_, WikiError>((id, doc))
            })
            .buffered(8)
            .try_collect()
            .await?;
        rows.retain(|row| {
            visible
                .get(&row.doc_id)
                .and_then(Option::as_ref)
                .is_some_and(|doc| {
                    doc.current_version == row.version_id
                        && doc.status == DOC_STATUS_ACTIVE
                        && access.allows(&doc.acl_label)
                })
        });
        if exclude_eval {
            // Production spaces hold no eval chunks (zero loss); inside
            // dedicated eval spaces this degrades to fewer hits, never
            // wrong ones.
            rows.retain(|row| row.namespace != EVAL_NAMESPACE);
        }

        let mut seen_docs = std::collections::BTreeSet::new();
        for row in &rows {
            seen_docs.insert(row.doc_id);
        }
        let total_docs_matched = seen_docs.len();

        let core: Vec<WikiChunkRecord> = match input.mode {
            WikiSearchMode::Chunks => rows,
            WikiSearchMode::Docs => {
                let mut picked = std::collections::BTreeSet::new();
                rows.into_iter()
                    .filter(|row| picked.insert(row.doc_id))
                    .take(top_k)
                    .collect()
            }
        };

        let expand = input.expand.unwrap_or(0).min(2) as usize;
        let hits = if expand == 0 {
            core.iter().map(|row| self.hit_from(row)).collect()
        } else {
            self.expand_hits(core, expand).await?
        };

        Ok(WikiSearchOutput {
            hits,
            total_docs_matched,
        })
    }

    /// Neighbor expansion (PRD §5.3): widens each hit by up to `expand`
    /// adjacent chunks. Chunks tile their version, so concatenating
    /// neighbors equals the exact content slice and the widened citation is
    /// recomputed over that range — still verifiable. Hits expand
    /// independently: nearby hits of one document may repeat overlapping
    /// context, each with its own verifiable citation.
    pub(super) async fn expand_hits(
        &self,
        core: Vec<WikiChunkRecord>,
        expand: usize,
    ) -> Result<Vec<WikiHit>, WikiError> {
        let versions: std::collections::BTreeSet<_> =
            core.iter().map(|row| row.version_id).collect();
        let layouts: BTreeMap<_, _> = futures::stream::iter(versions)
            .map(|id| async move {
                let version = self.version_record(id).await?;
                let plan = chunk_markdown(&version.content);
                Ok::<_, WikiError>((id, (version, plan)))
            })
            .buffered(8)
            .try_collect()
            .await?;
        let mut hits = Vec::with_capacity(core.len());
        for row in &core {
            let (version, plan) = &layouts[&row.version_id];
            let pos = row.ordinal as usize;
            let mut hit = self.hit_from(row);
            if pos < plan.drafts.len() {
                let lo = pos.saturating_sub(expand);
                let hi = (pos + expand).min(plan.drafts.len() - 1);
                let start = plan.drafts[lo].byte_start;
                let end = plan.drafts[hi].byte_end;
                hit.text = version.content[start..end].to_string();
                hit.citation.uri = citation_uri(
                    &self.space_id,
                    row.doc_id,
                    row.version_id,
                    start as u64,
                    end as u64,
                );
                hit.citation.byte_range = (start as u64, end as u64);
                hit.citation.checksum = chunk_checksum(&version.checksum, start, end, &hit.text);
            }
            hits.push(hit);
        }
        Ok(hits)
    }

    pub(super) fn hit_from(&self, row: &WikiChunkRecord) -> WikiHit {
        WikiHit {
            text: row.text.clone(),
            doc_title: row.title.clone(),
            heading_path: row.heading_path.clone(),
            citation: WikiCitation {
                uri: citation_uri(
                    &self.space_id,
                    row.doc_id,
                    row.version_id,
                    row.byte_start,
                    row.byte_end,
                ),
                doc_id: row.doc_id,
                version_id: row.version_id,
                chunk_id: row._id,
                heading_path: row.heading_path.clone(),
                anchor: row.anchor.clone(),
                byte_range: (row.byte_start, row.byte_end),
                checksum: row.checksum.clone(),
                quote: quote_excerpt(&row.text),
            },
        }
    }

    pub(super) async fn doc_ids_by_tags(&self, tags: &[String]) -> Result<Vec<u64>, WikiError> {
        self.scan_ids(
            &self.docs,
            Filter::Field((
                "tags".to_string(),
                RangeQuery::Include(
                    tags.iter()
                        .take(MAX_INCLUDE_KEYS)
                        .cloned()
                        .map(Fv::Text)
                        .collect(),
                ),
            )),
        )
        .await
    }
}
