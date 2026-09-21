//! OKF (Open Knowledge Format) v0.1 bundle import/export.
//!
//! Frontmatter is parsed as YAML mappings. Unknown key/value pairs are
//! preserved structurally; comments, key ordering and scalar formatting are
//! deliberately not part of the exchange contract. Native document fields
//! remain authoritative when exported.

use anda_db::schema::Json;
use std::collections::BTreeMap;

use super::{
    DEFAULT_NAMESPACE, DOC_STATUS_ACTIVE, EVENT_EXPORT_COMPLETED, EVENT_IMPORT_COMPLETED,
    WikiBundleEntry, WikiCommitInput, WikiDocInfo, WikiError, WikiExportOutput, WikiImportInput,
    WikiImportOutput, WikiImportSkip, WikiImportStatus, WikiImportedDoc, WikiListDocsInput,
    WikiService, markdown_title, prepare_commit, slugify_path,
};

pub const OKF_VERSION: &str = "0.1";
/// Metadata key holding unknown frontmatter key/value pairs.
pub const FRONTMATTER_KEY: &str = "x_okf_frontmatter";
/// Metadata key holding the OKF `type` value.
pub const OKF_TYPE_KEY: &str = "okf_type";
/// Sized so a full export of a large namespace replays in one call (M4
/// acceptance); bundles beyond this must be split by the external toolchain.
const MAX_IMPORT_ENTRIES: usize = 65_536;
/// Export ceiling, aligned with the import side: a namespace beyond this
/// cannot round-trip anyway and must be reorganized first.
const MAX_EXPORT_DOCS: usize = MAX_IMPORT_ENTRIES;

impl WikiService {
    /// Imports an OKF bundle into one namespace. Concept paths become
    /// hierarchical slugs; existing documents (same namespace + slug) are
    /// updated; identical content is a checksum-idempotent no-op, so
    /// re-importing a bundle never grows the version chain.
    pub async fn import_bundle(
        &self,
        actor: String,
        input: WikiImportInput,
        now_ms: u64,
    ) -> Result<WikiImportOutput, WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.import_bundle_inner(actor, input, now_ms).await })
            .await
    }

    async fn import_bundle_inner(
        &self,
        actor: String,
        input: WikiImportInput,
        now_ms: u64,
    ) -> Result<WikiImportOutput, WikiError> {
        if input.entries.is_empty() {
            return Err(WikiError::Invalid("bundle has no entries".into()));
        }
        if input.entries.len() > MAX_IMPORT_ENTRIES {
            return Err(WikiError::Invalid(format!(
                "bundle has {} entries, limit is {MAX_IMPORT_ENTRIES}",
                input.entries.len()
            )));
        }
        let namespace = input
            .namespace
            .as_deref()
            .map(str::trim)
            .filter(|ns| !ns.is_empty())
            .unwrap_or(DEFAULT_NAMESPACE)
            .to_string();

        let mut output = WikiImportOutput::default();
        // Distinct bundle paths that slugify to the same document ("A.md" vs
        // "a.md") converge last-write-wins into one document.
        for entry in &input.entries {
            match self.import_entry(&actor, &namespace, entry, now_ms).await {
                Ok(Some(doc)) => {
                    match doc.status {
                        WikiImportStatus::Created => output.created += 1,
                        WikiImportStatus::Updated => output.updated += 1,
                        WikiImportStatus::Unchanged => output.unchanged += 1,
                    }
                    output.docs.push(doc);
                }
                Ok(None) => output.skipped.push(WikiImportSkip {
                    path: entry.path.clone(),
                    reason: skip_reason(&entry.path),
                }),
                Err(err) => output.skipped.push(WikiImportSkip {
                    path: entry.path.clone(),
                    reason: err.to_string(),
                }),
            }
        }

        self.write_event(
            EVENT_IMPORT_COMPLETED,
            None,
            None,
            actor,
            BTreeMap::from([
                ("namespace".to_string(), Json::from(namespace)),
                ("created".to_string(), Json::from(output.created as u64)),
                ("updated".to_string(), Json::from(output.updated as u64)),
                ("unchanged".to_string(), Json::from(output.unchanged as u64)),
                (
                    "skipped".to_string(),
                    Json::from(output.skipped.len() as u64),
                ),
            ]),
            now_ms,
        )
        .await?;
        Ok(output)
    }

    async fn import_entry(
        &self,
        actor: &str,
        namespace: &str,
        entry: &WikiBundleEntry,
        now_ms: u64,
    ) -> Result<Option<WikiImportedDoc>, WikiError> {
        let Some(concept) = concept_path(&entry.path) else {
            return Ok(None);
        };
        let slug = slugify_path(&concept);

        let (frontmatter, body) = split_frontmatter(&entry.content);
        let mut fields = match frontmatter.as_deref() {
            Some(raw) => parse_frontmatter(raw)?,
            None => BTreeMap::new(),
        };
        let title = take_string(&mut fields, "title")?
            .or_else(|| markdown_title(body))
            .unwrap_or_else(|| concept.rsplit('/').next().unwrap_or(&concept).to_string());
        let tags = take_tags(&mut fields)?;
        let resource = take_string(&mut fields, "resource")?.unwrap_or_default();
        let kind = take_string(&mut fields, "type")?;
        let mut metadata = BTreeMap::new();
        if !fields.is_empty() {
            metadata.insert(
                FRONTMATTER_KEY.to_string(),
                serde_json::to_value(fields).map_err(|err| WikiError::Invalid(err.to_string()))?,
            );
        }
        if let Some(kind) = kind.filter(|kind| kind != "Document") {
            metadata.insert(OKF_TYPE_KEY.to_string(), Json::from(kind));
        }

        // Pure-CPU commit preparation (normalization, chunking, hashing)
        // stays outside the lock; document identity is resolved under it.
        let mut prepared = prepare_commit(WikiCommitInput {
            doc_id: None,
            parent_version: None,
            namespace: Some(namespace.to_string()),
            slug: Some(slug.clone()),
            title,
            content: body.to_string(),
            tags: Some(tags),
            // Exchange content cannot change native access control.
            acl_label: None,
            source_uri: Some(resource),
            message: Some(format!("okf import: {}", entry.path)),
            metadata: Some(metadata),
        })?;

        // "Slug lookup + commit" runs under one write-lock acquisition:
        // concurrent imports of the same bundle would otherwise both miss
        // the lookup and duplicate the document under a suffixed slug
        // instead of converging on one create + idempotent replays. The
        // lock is per entry, so large bundles do not starve other writers.
        let _guard = self.write_lock.lock().await;
        if let Some(id) = self.find_doc_id_by_slug(namespace, &slug).await? {
            let doc = self.doc_record(id).await?;
            prepared.input.doc_id = Some(doc._id);
            prepared.input.parent_version = Some(doc.current_version);
            // A file replaces the importer's fields, preserving unrelated host metadata.
            let imported = prepared.input.metadata.take().unwrap_or_default();
            let mut metadata = doc.metadata;
            metadata.remove(FRONTMATTER_KEY);
            metadata.remove(OKF_TYPE_KEY);
            metadata.extend(imported);
            prepared.input.metadata = Some(metadata);
            prepared.input.validate()?;
        }
        let out = self
            .commit_prepared(actor.to_string(), prepared, now_ms)
            .await?;
        Ok(Some(WikiImportedDoc {
            path: entry.path.clone(),
            doc_id: out.doc.id,
            version_id: out.version.id,
            status: if out.created {
                WikiImportStatus::Created
            } else if out.idempotent {
                WikiImportStatus::Unchanged
            } else {
                WikiImportStatus::Updated
            },
        }))
    }

    /// Exports one namespace as an OKF bundle: concept `.md` files (canonical
    /// frontmatter plus `x_anda_*` provenance keys), a root `index.md`, and
    /// a `manifest.json` with checksums so the bundle can be diffed and
    /// replayed.
    pub async fn export_bundle(
        &self,
        actor: String,
        namespace: Option<String>,
        now_ms: u64,
    ) -> Result<WikiExportOutput, WikiError> {
        let wiki = self.clone();
        self.owned(async move { wiki.export_bundle_inner(actor, namespace, now_ms).await })
            .await
    }

    async fn export_bundle_inner(
        &self,
        actor: String,
        namespace: Option<String>,
        now_ms: u64,
    ) -> Result<WikiExportOutput, WikiError> {
        let namespace = namespace
            .as_deref()
            .map(str::trim)
            .filter(|ns| !ns.is_empty())
            .unwrap_or(DEFAULT_NAMESPACE)
            .to_string();

        let mut docs: Vec<WikiDocInfo> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .list_docs(WikiListDocsInput {
                    namespace: Some(namespace.clone()),
                    status: Some(DOC_STATUS_ACTIVE.to_string()),
                    tag: None,
                    cursor: cursor.clone(),
                    limit: Some(100),
                })
                .await?;
            docs.extend(page.docs);
            if docs.len() > MAX_EXPORT_DOCS {
                // The whole namespace is buffered in memory and serialized
                // into a single response: refuse unbounded exports early.
                return Err(WikiError::Invalid(format!(
                    "namespace {namespace:?} has more than {MAX_EXPORT_DOCS} active documents; \
                     split content across namespaces and export them separately"
                )));
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        docs.sort_by(|a, b| a.slug.cmp(&b.slug));

        let mut entries = Vec::with_capacity(docs.len() + 2);
        let mut manifest_docs = Vec::with_capacity(docs.len());
        let mut index_lines = vec![
            "---".to_string(),
            format!("okf_version: \"{OKF_VERSION}\""),
            "---".to_string(),
            String::new(),
            format!("# {namespace}"),
            String::new(),
        ];

        for doc in &docs {
            let version = self.version_record(doc.current_version).await?;
            let frontmatter = render_frontmatter(doc, &version)?;
            let path = format!("{}.md", doc.slug);
            entries.push(WikiBundleEntry {
                path: path.clone(),
                content: format!("{frontmatter}{}", version.content),
            });
            index_lines.push(format!("- [{}]({})", doc.title, path));
            manifest_docs.push(serde_json::json!({
                "path": path,
                "title": doc.title,
                "doc_id": doc.id,
                "version_id": doc.current_version,
                "checksum": doc.current_checksum,
                "size": version.size,
            }));
        }

        index_lines.push(String::new());
        entries.push(WikiBundleEntry {
            path: "index.md".to_string(),
            content: index_lines.join("\n"),
        });
        let manifest = serde_json::json!({
            "okf_version": OKF_VERSION,
            "namespace": namespace,
            "exported_at": now_ms,
            "docs": manifest_docs,
        });
        entries.push(WikiBundleEntry {
            path: "manifest.json".to_string(),
            content: serde_json::to_string_pretty(&manifest)
                .map_err(|err| WikiError::Db(err.to_string()))?,
        });

        self.write_event(
            EVENT_EXPORT_COMPLETED,
            None,
            None,
            actor,
            BTreeMap::from([
                ("namespace".to_string(), Json::from(namespace.clone())),
                ("docs".to_string(), Json::from(docs.len() as u64)),
            ]),
            now_ms,
        )
        .await?;

        Ok(WikiExportOutput {
            namespace,
            entries,
            docs: docs.len(),
        })
    }
}

/// Bundle-relative concept path for an importable entry, or `None` for
/// reserved/non-markdown/invalid paths.
fn concept_path(path: &str) -> Option<String> {
    let path = path.trim().replace('\\', "/");
    let concept = path.strip_suffix(".md")?;
    if concept.is_empty() || path.starts_with('/') {
        return None;
    }
    let segments: Vec<&str> = concept.split('/').collect();
    if segments
        .iter()
        .any(|s| s.trim().is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    let base = segments.last()?.trim().to_ascii_lowercase();
    if base == "index" || base == "log" {
        return None; // reserved OKF files
    }
    Some(concept.to_string())
}

fn skip_reason(path: &str) -> String {
    let lower = path.trim().to_ascii_lowercase();
    if !lower.ends_with(".md") {
        return "not a markdown file".to_string();
    }
    if lower == "index.md"
        || lower == "log.md"
        || lower.ends_with("/index.md")
        || lower.ends_with("/log.md")
    {
        return "reserved OKF file".to_string();
    }
    "invalid path".to_string()
}

/// Splits an optional leading YAML frontmatter block. Returns the block
/// content without delimiters (LF-normalized)
/// and the body. Permissive: malformed frontmatter is treated as body.
pub(super) fn split_frontmatter(content: &str) -> (Option<String>, &str) {
    // The BOM is stripped in every branch so it never reaches the body (and
    // through it the checksum). `normalize_content` strips it again for
    // non-import commits.
    let stripped = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = match stripped.strip_prefix("---") {
        Some(rest) if rest.starts_with('\n') || rest.starts_with("\r\n") => rest,
        _ => return (None, stripped),
    };
    let block_start = if let Some(r) = rest.strip_prefix("\r\n") {
        r
    } else {
        &rest[1..]
    };

    let mut offset = 0usize;
    for line in block_start.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed == "---" || trimmed == "..." {
            let raw = &block_start[..offset];
            let body = &block_start[offset + line.len()..];
            let raw = raw.replace("\r\n", "\n").trim_end_matches('\n').to_string();
            return (Some(raw), body);
        }
        offset += line.len();
    }
    (None, stripped)
}

/// Parse the mapping once; YAML quoting, escapes and flow collections are
/// handled by the parser rather than by line splitting.
fn parse_frontmatter(raw: &str) -> Result<BTreeMap<String, Json>, WikiError> {
    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut fields: BTreeMap<String, Json> = serde_saphyr::from_str(raw)
        .map_err(|err| WikiError::Invalid(format!("invalid YAML frontmatter: {err}")))?;
    fields.retain(|key, _| !key.starts_with("x_anda_"));
    Ok(fields)
}

fn take_string(
    fields: &mut BTreeMap<String, Json>,
    key: &str,
) -> Result<Option<String>, WikiError> {
    match fields.remove(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(value)) => {
            Ok((!value.trim().is_empty()).then(|| value.trim().to_string()))
        }
        Some(_) => Err(WikiError::Invalid(format!(
            "frontmatter {key} must be a string"
        ))),
    }
}

fn take_tags(fields: &mut BTreeMap<String, Json>) -> Result<Vec<String>, WikiError> {
    match fields.remove("tags") {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(tags)) => tags
            .into_iter()
            .map(|tag| match tag {
                Json::String(tag) => Ok(tag),
                _ => Err(WikiError::Invalid(
                    "frontmatter tags must contain strings".into(),
                )),
            })
            .collect(),
        Some(_) => Err(WikiError::Invalid("frontmatter tags must be a list".into())),
    }
}

fn render_frontmatter(
    doc: &WikiDocInfo,
    version: &super::WikiVersionRecord,
) -> Result<String, WikiError> {
    let mut fields: BTreeMap<String, Json> = match doc.metadata.get(FRONTMATTER_KEY) {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|err| WikiError::Invalid(format!("invalid stored OKF fields: {err}")))?,
        None => BTreeMap::new(),
    };
    fields.retain(|key, _| !key.starts_with("x_anda_"));
    fields.insert(
        "type".into(),
        doc.metadata
            .get(OKF_TYPE_KEY)
            .cloned()
            .unwrap_or_else(|| "Document".into()),
    );
    fields.insert("title".into(), doc.title.clone().into());
    fields.remove("tags");
    if !doc.tags.is_empty() {
        fields.insert("tags".into(), serde_json::json!(doc.tags));
    }
    fields.remove("resource");
    if let Some(resource) = &doc.source_uri {
        fields.insert("resource".into(), resource.clone().into());
    }
    fields.insert("x_anda_doc_id".into(), doc.id.into());
    fields.insert("x_anda_version_id".into(), doc.current_version.into());
    fields.insert("x_anda_checksum".into(), version.checksum.clone().into());
    let raw = serde_saphyr::to_string(&fields).map_err(|err| WikiError::Db(err.to_string()))?;
    Ok(format!("---\n{}\n---\n", raw.trim_end_matches('\n')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_handles_quotes_comments_nested_values_and_folded_scalars() {
        let mut fields = parse_frontmatter("title: >\n  A long\n  title\ntags: ['research,development', 'true'] # comment\ncustom: {owner: platform, enabled: true}\nx_anda_fake: {ignore: me}\n").unwrap();
        assert_eq!(
            take_string(&mut fields, "title").unwrap().as_deref(),
            Some("A long title")
        );
        assert_eq!(
            take_tags(&mut fields).unwrap(),
            ["research,development", "true"]
        );
        assert_eq!(fields["custom"]["owner"], "platform");
        assert!(!fields.contains_key("x_anda_fake"));
    }

    #[test]
    fn malformed_or_wrongly_typed_frontmatter_is_an_error() {
        assert!(parse_frontmatter("tags: [broken").is_err());
        assert!(take_tags(&mut parse_frontmatter("tags: [true]").unwrap()).is_err());
        assert!(
            take_string(
                &mut parse_frontmatter("title: {nested: value}").unwrap(),
                "title"
            )
            .is_err()
        );
    }

    #[test]
    fn frontmatter_boundary_preserves_body() {
        let (raw, body) =
            split_frontmatter("\u{feff}---\r\ntitle: Guide\r\n---\r\n# Guide\nBody.\n");
        assert_eq!(raw.as_deref(), Some("title: Guide"));
        assert_eq!(body, "# Guide\nBody.\n");
    }

    #[test]
    fn concept_paths_exclude_reserved_and_unsafe_names() {
        for path in [
            "index.md",
            "log.md",
            "a/index.md",
            "../d.md",
            "/d.md",
            "a//b.md",
            "a.json",
        ] {
            assert!(concept_path(path).is_none(), "{path}");
        }
        assert_eq!(
            concept_path("guides/setup.md").as_deref(),
            Some("guides/setup")
        );
    }
}
