//! Authorized document snapshots and source-derived reads.

use super::*;

impl WikiService {
    /// Read counterpart of [`WikiService::search_view`]: denials read as
    /// NotFound, exactly like the scoped HTTP path.
    pub async fn read_view(
        &self,
        input: WikiReadInput,
        view: Option<&[String]>,
    ) -> Result<WikiReadOutput, WikiError> {
        let access = WikiAccess {
            actor: String::new(),
            labels: view.map(<[String]>::to_vec),
        };
        let doc = self.doc_record(input.doc_id).await?;
        self.guard_doc(&doc, &access)?;
        self.read_document(&doc, input).await
    }

    /// Test convenience; production reads always resolve an authorized snapshot.
    #[cfg(test)]
    pub(super) async fn read(&self, input: WikiReadInput) -> Result<WikiReadOutput, WikiError> {
        let doc = self.doc_record(input.doc_id).await?;
        self.read_document(&doc, input).await
    }

    /// Authorization and version selection share this immutable document snapshot.
    pub(super) async fn read_document(
        &self,
        doc: &WikiDocRecord,
        input: WikiReadInput,
    ) -> Result<WikiReadOutput, WikiError> {
        let version_id = input.version.unwrap_or(doc.current_version);
        let version = self.published_version(doc, version_id).await?;
        let is_current = version_id == doc.current_version;
        let content = &version.content;

        let mut output = WikiReadOutput {
            doc_id: doc._id,
            version_id,
            is_current,
            title: doc.title.clone(),
            status: doc.status.clone(),
            checksum: version.checksum.clone(),
            size: version.size,
            toc: None,
            content: None,
            byte_range: None,
            truncated: false,
        };

        match input.selector {
            WikiSelector::Toc => output.toc = Some(outline::outline(content)),
            WikiSelector::Section { anchor } => {
                let layout = outline::outline(content);
                let entry = layout
                    .iter()
                    .find(|entry| entry.anchor == anchor)
                    .ok_or_else(|| {
                        WikiError::NotFound(format!("section anchor {anchor} not found"))
                    })?;
                let start = entry.byte_start as usize;
                let end = floor_char_boundary(
                    content,
                    (entry.byte_end as usize).min(start + MAX_READ_BYTES),
                );
                output.truncated = end < entry.byte_end as usize;
                output.content = Some(content[start..end].to_string());
                output.byte_range = Some((start as u64, end as u64));
            }
            WikiSelector::Range { start, end } => {
                if start > end {
                    return Err(WikiError::Invalid("range start exceeds end".into()));
                }
                let start = floor_char_boundary(content, start as usize);
                let mut end = floor_char_boundary(content, end as usize);
                // Bounded like Full: Range{0, u64::MAX} must not bypass the
                // read cap.
                if end - start > MAX_READ_BYTES {
                    end = floor_char_boundary(content, start + MAX_READ_BYTES);
                    output.truncated = true;
                }
                output.content = Some(content[start..end].to_string());
                output.byte_range = Some((start as u64, end as u64));
            }
            WikiSelector::Full => {
                let end = floor_char_boundary(content, MAX_READ_BYTES);
                output.truncated = end < content.len();
                output.content = Some(content[..end].to_string());
                output.byte_range = Some((0, end as u64));
            }
        }
        Ok(output)
    }

    /// Citation verification: recomputes the chunk checksum from the
    /// immutable version content. `Invalid` means the citation does not
    /// match stored content. `Superseded` reports the version that replaced
    /// the cited one. Mismatches are never evented: any anonymous caller
    /// could otherwise flood the audit log through `/wiki/verify`.
    #[cfg(test)]
    pub(super) async fn verify(
        &self,
        actor: String,
        input: WikiVerifyInput,
        now_ms: u64,
    ) -> Result<WikiVerifyOutput, WikiError> {
        self.verify_scoped(
            &WikiAccess {
                actor,
                labels: None,
            },
            input,
            now_ms,
        )
        .await
    }

    /// Resolves the verification target: URI form first, explicit fields
    /// otherwise.
    pub(super) fn verify_target(
        &self,
        input: &WikiVerifyInput,
    ) -> Result<(u64, u64, u64, u64), WikiError> {
        match &input.uri {
            Some(uri) => {
                let (space, doc_id, version_id, start, end) = parse_citation_uri(uri)
                    .ok_or_else(|| WikiError::Invalid(format!("malformed citation uri: {uri}")))?;
                if space != self.space_id {
                    return Err(WikiError::Invalid(format!(
                        "citation belongs to space {space}, not {}",
                        self.space_id
                    )));
                }
                Ok((doc_id, version_id, start, end))
            }
            None => {
                let (Some(doc_id), Some(version_id), Some((start, end))) =
                    (input.doc_id, input.version_id, input.byte_range)
                else {
                    return Err(WikiError::Invalid(
                        "either uri or doc_id+version_id+byte_range is required".into(),
                    ));
                };
                Ok((doc_id, version_id, start, end))
            }
        }
    }

    /// Verification core over preloaded records, so batch callers (the
    /// digest citation sample) can reuse one (doc, version) load across
    /// many facts instead of re-reading version content per fact.
    pub(super) async fn verify_resolved(
        &self,
        _actor: String,
        doc: &WikiDocRecord,
        version: &WikiVersionRecord,
        (start, end): (u64, u64),
        expected: Option<&str>,
        _now_ms: u64,
    ) -> Result<WikiVerifyOutput, WikiError> {
        if version.doc_id != doc._id {
            return Ok(verify_not_found());
        }
        let Some(text) = version.content.get(start as usize..end as usize) else {
            return Ok(WikiVerifyOutput {
                status: WikiVerifyStatus::Invalid,
                current_version: Some(doc.current_version),
                checksum: None,
                quote: None,
            });
        };

        let computed = chunk_checksum(&version.checksum, start as usize, end as usize, text);
        if let Some(expected) = expected
            && expected != computed
        {
            return Ok(WikiVerifyOutput {
                status: WikiVerifyStatus::Invalid,
                current_version: Some(doc.current_version),
                checksum: Some(computed),
                quote: None,
            });
        }

        Ok(WikiVerifyOutput {
            status: if version._id == doc.current_version {
                WikiVerifyStatus::Valid
            } else {
                WikiVerifyStatus::Superseded
            },
            current_version: Some(doc.current_version),
            checksum: Some(computed),
            quote: Some(quote_excerpt(text)),
        })
    }

    /// Test-only doc lookup; production paths authorize document snapshots.
    #[cfg(test)]
    pub(crate) async fn get_doc(&self, doc_id: u64) -> Result<WikiDocInfo, WikiError> {
        Ok(self.doc_record(doc_id).await?.into())
    }

    pub(super) async fn list_docs(
        &self,
        input: WikiListDocsInput,
    ) -> Result<WikiDocListOutput, WikiError> {
        self.list_docs_inner(input, None).await
    }

    pub(super) async fn list_docs_inner(
        &self,
        input: WikiListDocsInput,
        labels: Option<&[String]>,
    ) -> Result<WikiDocListOutput, WikiError> {
        let limit = input.limit.unwrap_or(20).clamp(1, 100);
        let cursor = self.cursor_or_max(&self.docs, &input.cursor)?;

        let mut filters: Vec<Box<Filter>> = vec![
            Box::new(Filter::Field((
                "_id".to_string(),
                RangeQuery::Lt(Fv::U64(cursor)),
            ))),
            // Hide initializing sentinels (crashed creates awaiting sweep).
            Box::new(Filter::Field((
                "current_version".to_string(),
                RangeQuery::Gt(Fv::U64(0)),
            ))),
        ];
        if let Some(namespace) = &input.namespace {
            filters.push(Box::new(Filter::Field((
                "namespace".to_string(),
                RangeQuery::Eq(Fv::Text(namespace.clone())),
            ))));
        }
        if let Some(status) = &input.status {
            filters.push(Box::new(Filter::Field((
                "status".to_string(),
                RangeQuery::Eq(Fv::Text(status.clone())),
            ))));
        }
        if let Some(tag) = &input.tag {
            filters.push(Box::new(Filter::Field((
                "tags".to_string(),
                RangeQuery::Eq(Fv::Text(tag.clone())),
            ))));
        }
        if let Some(labels) = labels {
            filters.push(Box::new(acl_filter(labels)));
        }

        let mut rows: Vec<WikiDocRecord> =
            query_last_as(&self.docs, Filter::And(filters), limit).await?;
        let next_cursor = page_cursor(&rows, limit, |doc| doc._id);
        rows.retain(|doc| {
            doc.current_version > 0
                && input
                    .namespace
                    .as_ref()
                    .is_none_or(|namespace| *namespace == doc.namespace)
                && input
                    .status
                    .as_ref()
                    .is_none_or(|status| *status == doc.status)
                && input.tag.as_ref().is_none_or(|tag| doc.tags.contains(tag))
                && labels.is_none_or(|labels| {
                    WikiAccess {
                        actor: String::new(),
                        labels: Some(labels.to_vec()),
                    }
                    .allows(&doc.acl_label)
                })
        });
        Ok(WikiDocListOutput {
            docs: rows.into_iter().map(Into::into).collect(),
            next_cursor,
        })
    }

    #[cfg(test)]
    pub(super) async fn list_versions(
        &self,
        doc_id: u64,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<WikiVersionListOutput, WikiError> {
        let doc = self.doc_record(doc_id).await?;
        self.document_versions(&doc, cursor, limit).await
    }

    pub(super) fn guard_doc<'a>(
        &self,
        doc: &'a WikiDocRecord,
        access: &WikiAccess,
    ) -> Result<&'a WikiDocRecord, WikiError> {
        if access.allows(&doc.acl_label) {
            Ok(doc)
        } else {
            // NotFound, not Forbidden: restricted tokens must not learn that
            // a labeled document exists.
            Err(WikiError::NotFound(format!(
                "document {} not found",
                doc._id
            )))
        }
    }

    #[cfg(test)]
    pub async fn get_doc_scoped(
        &self,
        access: &WikiAccess,
        doc_id: u64,
    ) -> Result<WikiDocInfo, WikiError> {
        let doc = self.doc_record(doc_id).await?;
        self.guard_doc(&doc, access)?;
        Ok(doc.into())
    }

    pub async fn document_scoped(
        &self,
        access: &WikiAccess,
        doc_id: u64,
        now_ms: u64,
    ) -> Result<(WikiDocInfo, Vec<WikiTocEntry>), WikiError> {
        let doc = self.doc_record(doc_id).await?;
        self.guard_doc(&doc, access)?;
        let output = self
            .read_document(
                &doc,
                WikiReadInput {
                    doc_id,
                    version: None,
                    selector: WikiSelector::Toc,
                },
            )
            .await?;
        self.audit_read(access, &output, now_ms).await;
        Ok((doc.into(), output.toc.unwrap_or_default()))
    }

    pub async fn read_scoped(
        &self,
        access: &WikiAccess,
        input: WikiReadInput,
        now_ms: u64,
    ) -> Result<WikiReadOutput, WikiError> {
        let doc = self.doc_record(input.doc_id).await?;
        self.guard_doc(&doc, access)?;
        let output = self.read_document(&doc, input).await?;
        self.audit_read(access, &output, now_ms).await;
        Ok(output)
    }

    pub(super) async fn audit_read(
        &self,
        access: &WikiAccess,
        output: &WikiReadOutput,
        now_ms: u64,
    ) {
        if self.audit_reads() {
            let _ = self
                .audit_event(
                    EVENT_WIKI_READ,
                    Some(output.doc_id),
                    Some(output.version_id),
                    access.actor.clone(),
                    BTreeMap::from([(
                        "restricted".to_string(),
                        Json::from(access.labels.is_some()),
                    )]),
                    now_ms,
                )
                .await;
        }
    }

    pub async fn list_docs_scoped(
        &self,
        access: &WikiAccess,
        input: WikiListDocsInput,
    ) -> Result<WikiDocListOutput, WikiError> {
        self.list_docs_inner(input, access.labels.as_deref()).await
    }

    pub async fn list_versions_scoped(
        &self,
        access: &WikiAccess,
        doc_id: u64,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<WikiVersionListOutput, WikiError> {
        let doc = self.doc_record(doc_id).await?;
        self.guard_doc(&doc, access)?;
        self.document_versions(&doc, cursor, limit).await
    }

    pub async fn verify_scoped(
        &self,
        access: &WikiAccess,
        input: WikiVerifyInput,
        now_ms: u64,
    ) -> Result<WikiVerifyOutput, WikiError> {
        let (doc_id, version_id, start, end) = self.verify_target(&input)?;
        let doc = match self.doc_record(doc_id).await {
            Ok(doc) if access.allows(&doc.acl_label) => doc,
            Ok(_) | Err(WikiError::NotFound(_)) => return Ok(verify_not_found()),
            Err(err) => return Err(err),
        };
        let version = match self.published_version(&doc, version_id).await {
            Ok(version) => version,
            Err(WikiError::NotFound(_)) => return Ok(verify_not_found()),
            Err(err) => return Err(err),
        };
        self.verify_resolved(
            access.actor.clone(),
            &doc,
            &version,
            (start, end),
            input.checksum.as_deref(),
            now_ms,
        )
        .await
    }
}
