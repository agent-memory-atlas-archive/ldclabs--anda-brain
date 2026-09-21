//! End-to-end wiki invariants, using local stores only.
use super::tests::{commit_input, test_wiki};
use super::*;

#[derive(Debug, Default)]
struct ReadGate {
    inner: object_store::memory::InMemory,
    armed: std::sync::atomic::AtomicBool,
    armed_write: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl std::fmt::Display for ReadGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadGate")
    }
}
#[async_trait::async_trait]
impl object_store::ObjectStore for ReadGate {
    async fn put_opts(
        &self,
        path: &object_store::path::Path,
        payload: object_store::PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        let result = self.inner.put_opts(path, payload, options).await?;
        if path.as_ref().contains("/wiki_versions/data/")
            && self
                .armed_write
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(result)
    }
    async fn put_multipart_opts(
        &self,
        path: &object_store::path::Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }
    async fn get_opts(
        &self,
        path: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        let result = self.inner.get_opts(path, options).await?;
        if path.as_ref().contains("/wiki_docs/data/")
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(result)
    }
    fn delete_stream(
        &self,
        paths: futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(paths)
    }
    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn read_does_not_return_new_restricted_body() {
    let store = Arc::new(ReadGate::default());
    let mut config = crate::testkit::db_config("review_acl_read");
    config.storage.cache_max_capacity = 0;
    let db = Arc::new(AndaDB::create(store.clone(), config).await.unwrap());
    let wiki = Arc::new(
        WikiService::connect("review_space".into(), db)
            .await
            .unwrap(),
    );
    let first = wiki
        .commit(
            "a".into(),
            commit_input("Guide", "# Guide\nPublic instructions.\n"),
            1000,
        )
        .await
        .unwrap();
    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let reader_wiki = wiki.clone();
    let reader = tokio::spawn(async move {
        reader_wiki
            .read_scoped(
                &WikiAccess {
                    actor: "public".into(),
                    labels: Some(vec![]),
                },
                WikiReadInput {
                    doc_id: first.doc.id,
                    version: None,
                    selector: WikiSelector::Full,
                },
                2000,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), store.entered.notified())
        .await
        .unwrap();
    let mut update = commit_input(
        "Private Guide",
        "# Private Guide\nNEW SECRET NEVER PUBLIC.\n",
    );
    update.doc_id = Some(first.doc.id);
    update.parent_version = Some(first.version.id);
    update.acl_label = Some("secret".into());
    wiki.commit("a".into(), update, 3000).await.unwrap();
    store.release.notify_one();
    let result = reader.await.unwrap();
    assert!(
        result.as_ref().map_or(true, |output| !output
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("NEW SECRET")),
        "public read returned private replacement: {result:?}"
    );
}

#[tokio::test]
async fn toc_preserves_short_document_sections() {
    let wiki = test_wiki("review_toc").await;
    let content =
        "# Guide\n\nIntroduction.\n\n## Install\n\nRun setup.\n\n## Remove\n\nRun uninstall.\n";
    let committed = wiki
        .commit("a".into(), commit_input("Guide", content), 1000)
        .await
        .unwrap();
    let toc = wiki
        .read_scoped(
            &WikiAccess {
                actor: "a".into(),
                labels: None,
            },
            WikiReadInput {
                doc_id: committed.doc.id,
                version: None,
                selector: WikiSelector::Toc,
            },
            2000,
        )
        .await
        .unwrap()
        .toc
        .unwrap();
    assert!(
        toc.iter()
            .any(|s| s.heading_path.last().is_some_and(|s| s == "Install")),
        "TOC: {toc:?}"
    );
}

#[tokio::test]
async fn okf_roundtrip_preserves_comma_in_tag() {
    let wiki = test_wiki("review_okf_tag").await;
    let mut input = commit_input("Guide", "# Guide\n\nInstructions.\n");
    input.tags = Some(vec!["research,development".into()]);
    let committed = wiki.commit("a".into(), input, 1000).await.unwrap();
    let exported = wiki.export_bundle("a".into(), None, 2000).await.unwrap();
    wiki.import_bundle(
        "a".into(),
        WikiImportInput {
            entries: exported.entries,
            namespace: None,
        },
        3000,
    )
    .await
    .unwrap();
    let after = wiki.get_doc(committed.doc.id).await.unwrap();
    assert_eq!(
        after.tags,
        ["research,development"],
        "plain export/import changed tags"
    );
}

#[tokio::test]
async fn okf_title_edit_preserves_custom_fields() {
    let wiki = test_wiki("review_okf_custom").await;
    let imported = wiki.import_bundle("a".into(), WikiImportInput { namespace: None, entries: vec![WikiBundleEntry {
        path: "guide.md".into(), content: "---\ntitle: Guide\nowner: platform\ncustom_field: keep-me\n---\n# Guide\n\nInstructions.\n".into(),
    }] }, 1000).await.unwrap();
    let original = &imported.docs[0];
    let mut input = commit_input("Renamed Guide", "# Guide\n\nInstructions.\n");
    input.doc_id = Some(original.doc_id);
    input.parent_version = Some(original.version_id);
    wiki.commit("a".into(), input, 2000).await.unwrap();
    let exported = wiki.export_bundle("a".into(), None, 3000).await.unwrap();
    let guide = exported
        .entries
        .iter()
        .find(|e| e.path == "guide.md")
        .unwrap();
    assert!(
        guide.content.contains("custom_field: keep-me"),
        "exported: {}",
        guide.content
    );
}

#[tokio::test]
async fn reimport_removes_deleted_frontmatter() {
    let wiki = test_wiki("review_okf_delete").await;
    let entry = |content: &str| WikiImportInput {
        namespace: None,
        entries: vec![WikiBundleEntry {
            path: "guide.md".into(),
            content: content.into(),
        }],
    };
    wiki.import_bundle("a".into(), entry("---\ntitle: Guide\ncustom_field: deleted-value\nresource: https://example.org/old\n---\n# Guide\nText.\n"), 1000).await.unwrap();
    wiki.import_bundle("a".into(), entry("# Guide\nText.\n"), 2000)
        .await
        .unwrap();
    let exported = wiki.export_bundle("a".into(), None, 3000).await.unwrap();
    let guide = exported
        .entries
        .iter()
        .find(|e| e.path == "guide.md")
        .unwrap();
    assert!(
        !guide.content.contains("deleted-value")
            && !guide.content.contains("https://example.org/old"),
        "deleted fields resurrected: {}",
        guide.content
    );
}

#[tokio::test]
async fn acl_search_uses_current_document_label() {
    let wiki = test_wiki("review_acl_snapshot").await;
    let original = wiki
        .commit(
            "a".into(),
            commit_input("Guide", "# Guide\nInternal configuration.\n"),
            1000,
        )
        .await
        .unwrap();
    // Exact ordinary commit step 3: registry has the new label, old chunks
    // still exist and remain current until steps 4/5. No corrupt storage.
    wiki.docs
        .update(
            original.doc.id,
            BTreeMap::from([("acl_label".into(), Fv::Text("internal".into()))]),
        )
        .await
        .unwrap();
    let output = wiki
        .search_scoped(
            &WikiAccess {
                actor: "public".into(),
                labels: Some(vec![]),
            },
            WikiSearchInput::from_query("configuration".into()),
            2000,
        )
        .await
        .unwrap();
    assert!(
        output.hits.is_empty(),
        "restricted search returned old public chunks: {output:?}"
    );
}

#[tokio::test]
async fn failed_update_stays_out_of_history_after_retry() {
    let wiki = test_wiki("review_orphan_retry").await;
    let first = wiki
        .commit(
            "a".into(),
            commit_input("Guide", "# Guide\nOriginal.\n"),
            1000,
        )
        .await
        .unwrap();
    // Failed update after version-row storage, before publishing the document.
    let uncommitted = wiki
        .versions
        .add_from(&WikiVersionRecord {
            _id: 0,
            doc_id: first.doc.id,
            parent_version: Some(first.version.id),
            checksum: "uncommitted".into(),
            content: "Never published.\n".into(),
            size: 17,
            author: "a".into(),
            message: None,
            created_at: 2000,
        })
        .await
        .unwrap();
    let mut retry = commit_input("Guide", "# Guide\nSuccessfully retried.\n");
    retry.doc_id = Some(first.doc.id);
    retry.parent_version = Some(first.version.id);
    let current = wiki.commit("a".into(), retry, 3000).await.unwrap();
    assert!(
        wiki.read(WikiReadInput {
            doc_id: first.doc.id,
            version: Some(uncommitted),
            selector: WikiSelector::Full,
        })
        .await
        .is_err()
    );
    assert_eq!(
        wiki.verify(
            "a".into(),
            WikiVerifyInput {
                doc_id: Some(first.doc.id),
                version_id: Some(uncommitted),
                byte_range: Some((0, 4)),
                ..Default::default()
            },
            3500
        )
        .await
        .unwrap()
        .status,
        WikiVerifyStatus::NotFound
    );
    wiki.orphan_sweep(4000).await.unwrap();
    let history = wiki
        .list_versions(first.doc.id, None, Some(100))
        .await
        .unwrap();
    assert_eq!(
        history.versions.iter().map(|v| v.id).collect::<Vec<_>>(),
        [first.version.id, current.version.id],
        "failed version {uncommitted} became history"
    );
}

#[tokio::test]
async fn normal_close_reopen_preserves_wiki() {
    let store = Arc::new(object_store::memory::InMemory::new());
    let config = crate::testkit::db_config("review_wiki_restart");
    let db = Arc::new(AndaDB::create(store.clone(), config.clone()).await.unwrap());
    let wiki = WikiService::connect("review_space".into(), db.clone())
        .await
        .unwrap();
    let first = wiki
        .commit(
            "a".into(),
            commit_input("Guide", "# Guide\nOriginal.\n"),
            1000,
        )
        .await
        .unwrap();
    let mut update = commit_input("Guide", "# Guide\nRestart verification.\n");
    update.doc_id = Some(first.doc.id);
    update.parent_version = Some(first.version.id);
    let current = wiki.commit("a".into(), update, 2000).await.unwrap();
    wiki.archive("a".into(), first.doc.id, 3000).await.unwrap();
    db.close().await.unwrap();
    drop(wiki);
    drop(db);
    let reopened = Arc::new(AndaDB::open(store, config).await.unwrap());
    let wiki = WikiService::connect("review_space".into(), reopened.clone())
        .await
        .unwrap();
    wiki.orphan_sweep(4000).await.unwrap();
    assert_eq!(
        wiki.doc_record(first.doc.id).await.unwrap().status,
        DOC_STATUS_ARCHIVED
    );
    assert_eq!(
        wiki.version_record(current.version.id)
            .await
            .unwrap()
            .content,
        "# Guide\nRestart verification.\n"
    );
    assert!(
        wiki.search(WikiSearchInput::from_query("Restart".into()))
            .await
            .unwrap()
            .hits
            .is_empty()
    );
    wiki.restore("a".into(), first.doc.id, 5000).await.unwrap();
    assert_eq!(
        wiki.search(WikiSearchInput::from_query("Restart".into()))
            .await
            .unwrap()
            .hits
            .len(),
        1
    );
    assert_eq!(
        wiki.list_versions(first.doc.id, None, Some(100))
            .await
            .unwrap()
            .versions
            .len(),
        2
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_commit_drains_before_close_and_survives_reopen() {
    let store = Arc::new(ReadGate::default());
    let config = crate::testkit::db_config("wiki_cancelled_commit");
    let db = Arc::new(AndaDB::create(store.clone(), config.clone()).await.unwrap());
    let wiki = Arc::new(
        WikiService::connect("test_space".into(), db.clone())
            .await
            .unwrap(),
    );
    store
        .armed_write
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let writer_wiki = wiki.clone();
    let waiter = tokio::spawn(async move {
        writer_wiki
            .commit(
                "a".into(),
                commit_input("Durable", "# Durable\nRetained content.\n"),
                1000,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), store.entered.notified())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(wiki.is_busy());
    store.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), wiki.shutdown())
        .await
        .unwrap();
    assert!(!wiki.is_busy());
    assert!(
        wiki.commit("a".into(), commit_input("Late", "Rejected.\n"), 2000)
            .await
            .is_err()
    );
    db.close().await.unwrap();
    drop(wiki);
    drop(db);
    let reopened = Arc::new(AndaDB::open(store, config).await.unwrap());
    let wiki = WikiService::connect("test_space".into(), reopened.clone())
        .await
        .unwrap();
    let hits = wiki
        .search(WikiSearchInput::from_query("Retained".into()))
        .await
        .unwrap();
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(
        wiki.list_docs(WikiListDocsInput::default())
            .await
            .unwrap()
            .docs
            .len(),
        1
    );
    assert!(wiki.orphan_sweep(3000).await.unwrap().is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn section_reads_are_independent_of_retrieval_packing_and_include_children() {
    let wiki = test_wiki("wiki_sections").await;
    let content =
        "# Guide\nIntro.\n## Install\nRun setup.\n### Linux\nUse apt.\n## Remove\nRun uninstall.\n";
    let out = wiki
        .commit("a".into(), commit_input("Guide", content), 1000)
        .await
        .unwrap();
    let section = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Section {
                anchor: "install".into(),
            },
        })
        .await
        .unwrap();
    assert_eq!(
        section.content.as_deref(),
        Some("## Install\nRun setup.\n### Linux\nUse apt.\n")
    );
    let mut update = commit_input(
        "Guide",
        &format!(
            "# Guide\n{}\n## Install\nNew text.\n",
            "Long introduction.\n\n".repeat(200)
        ),
    );
    update.doc_id = Some(out.doc.id);
    update.parent_version = Some(out.version.id);
    wiki.commit("a".into(), update, 2000).await.unwrap();
    let historical = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: Some(out.version.id),
            selector: WikiSelector::Section {
                anchor: "install".into(),
            },
        })
        .await
        .unwrap();
    assert_eq!(historical.content, section.content);
    let current = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Section {
                anchor: "install".into(),
            },
        })
        .await
        .unwrap();
    assert_eq!(current.content.as_deref(), Some("## Install\nNew text.\n"));
}

#[tokio::test]
async fn import_replaces_only_exchange_owned_metadata() {
    let wiki = test_wiki("wiki_import_metadata").await;
    let mut input = commit_input("Guide", "# Guide\nText.\n");
    input.metadata = Some(BTreeMap::from([("host_key".into(), Json::from("keep"))]));
    input.acl_label = Some("private".into());
    let created = wiki.commit("a".into(), input, 1000).await.unwrap();
    for content in [
        "---\ntitle: Guide\ntype: Manual\ncustom: {key: value}\nresource: https://example.com\n---\n# Guide\nText.\n",
        "# Guide\nText.\n",
    ] {
        let imported = wiki
            .import_bundle(
                "a".into(),
                WikiImportInput {
                    namespace: None,
                    entries: vec![WikiBundleEntry {
                        path: "guide.md".into(),
                        content: content.into(),
                    }],
                },
                2000,
            )
            .await
            .unwrap();
        assert!(imported.skipped.is_empty());
    }
    let doc = wiki.get_doc(created.doc.id).await.unwrap();
    assert_eq!(
        doc.metadata,
        BTreeMap::from([("host_key".into(), Json::from("keep"))])
    );
    assert_eq!(doc.acl_label, "private");
    assert!(doc.source_uri.is_none());
}
