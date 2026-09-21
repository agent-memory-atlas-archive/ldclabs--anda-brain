use super::tests::{commit_input, test_wiki};
use super::*;

fn bundle_entry(path: &str, content: &str) -> WikiBundleEntry {
    WikiBundleEntry {
        path: path.to_string(),
        content: content.to_string(),
    }
}

#[tokio::test]
async fn okf_import_maps_frontmatter_and_paths() {
    let wiki = test_wiki("wiki_okf_import").await;
    let out = wiki
        .import_bundle(
            "importer".to_string(),
            WikiImportInput {
                entries: vec![
                    bundle_entry(
                        "guides/setup.md",
                        "---\ntype: Guide\ntitle: 安装指南\ntags: [setup, 中文]\nresource: https://example.com/setup\ncustom_field: 保留我\n---\n\n# 安装指南\n\n准备环境并执行安装脚本。\n",
                    ),
                    bundle_entry("index.md", "# listing"),
                    bundle_entry("notes/log.md", "history"),
                    bundle_entry("manifest.json", "{}"),
                    bundle_entry("../evil.md", "# nope"),
                ],
                namespace: Some("kb".to_string()),
            },
            1000,
        )
        .await
        .unwrap();

    assert_eq!(out.created, 1);
    assert_eq!(out.skipped.len(), 4);
    let doc = wiki.get_doc(out.docs[0].doc_id).await.unwrap();
    assert_eq!(doc.namespace, "kb");
    assert_eq!(doc.slug, "guides/setup");
    assert_eq!(doc.title, "安装指南");
    assert_eq!(doc.tags, vec!["setup".to_string(), "中文".to_string()]);
    assert_eq!(doc.source_uri.as_deref(), Some("https://example.com/setup"));
    assert_eq!(doc.metadata["x_okf_frontmatter"]["custom_field"], "保留我");

    // Content excludes frontmatter: body starts at the heading.
    let read = wiki
        .read(WikiReadInput {
            doc_id: doc.id,
            version: None,
            selector: WikiSelector::Full,
        })
        .await
        .unwrap();
    assert!(read.content.unwrap().trim_start().starts_with("# 安装指南"));
}

#[tokio::test]
async fn okf_round_trip_preserves_unknown_fields_with_zero_growth() {
    let wiki = test_wiki("wiki_okf_roundtrip").await;
    let entries = vec![
        bundle_entry(
            "policy/security.md",
            "---\ntype: Policy\ntitle: 安全政策\n# reviewer: alice — keep this comment\nunknown_key: 未知字段无损\ntags: [policy]\n---\n\n# 安全政策\n\n密钥必须存放在 KMS。\n",
        ),
        bundle_entry("faq.md", "# 常见问题\n\n没有 frontmatter 的文档。\n"),
    ];
    let input = WikiImportInput {
        entries: entries.clone(),
        namespace: Some("kb".to_string()),
    };

    let first = wiki
        .import_bundle("importer".to_string(), input.clone(), 1000)
        .await
        .unwrap();
    assert_eq!(first.created, 2);

    // Re-import: checksum-idempotent, zero version growth.
    let second = wiki
        .import_bundle("importer".to_string(), input.clone(), 2000)
        .await
        .unwrap();
    assert_eq!(second.unchanged, 2);
    assert_eq!(second.created + second.updated, 0);
    for doc in &first.docs {
        let versions = wiki
            .list_versions(doc.doc_id, None, Some(10))
            .await
            .unwrap();
        assert_eq!(versions.versions.len(), 1);
    }

    // Export preserves unknown values; YAML formatting/comments are canonicalized.
    let export = wiki
        .export_bundle("exporter".to_string(), Some("kb".to_string()), 3000)
        .await
        .unwrap();
    assert_eq!(export.docs, 2);
    let sec = export
        .entries
        .iter()
        .find(|e| e.path == "policy/security.md")
        .unwrap();
    assert!(sec.content.contains("unknown_key: 未知字段无损"));
    assert!(!sec.content.contains("# reviewer:"));
    assert!(sec.content.contains("x_anda_doc_id:"));
    assert!(sec.content.contains("x_anda_checksum: sha3-256:"));
    assert!(export.entries.iter().any(|e| e.path == "index.md"));
    let manifest = export
        .entries
        .iter()
        .find(|e| e.path == "manifest.json")
        .unwrap();
    assert!(manifest.content.contains("\"okf_version\": \"0.1\""));

    // Import the exported bundle back: zero growth, including documents
    // that originally had no frontmatter.
    let reimport = wiki
        .import_bundle(
            "importer".to_string(),
            WikiImportInput {
                entries: export.entries.clone(),
                namespace: Some("kb".to_string()),
            },
            4000,
        )
        .await
        .unwrap();
    assert_eq!(reimport.created, 0);
    assert_eq!(reimport.unchanged, 2);
    assert_eq!(reimport.updated, 0);
    // The OKF-origin doc must be byte-stable across the full cycle.
    let sec_status = reimport
        .docs
        .iter()
        .find(|d| d.path == "policy/security.md")
        .unwrap();
    assert_eq!(sec_status.status, WikiImportStatus::Unchanged);

    // A second full cycle is completely stable for every doc.
    let export2 = wiki
        .export_bundle("exporter".to_string(), Some("kb".to_string()), 5000)
        .await
        .unwrap();
    let reimport2 = wiki
        .import_bundle(
            "importer".to_string(),
            WikiImportInput {
                entries: export2.entries,
                namespace: Some("kb".to_string()),
            },
            6000,
        )
        .await
        .unwrap();
    assert_eq!(reimport2.unchanged, 2);
    assert_eq!(reimport2.created + reimport2.updated, 0);
}

#[tokio::test]
async fn okf_export_syncs_title_tags_drifted_by_commits() {
    let wiki = test_wiki("wiki_okf_drift").await;
    let imported = wiki
        .import_bundle(
            "i".to_string(),
            WikiImportInput {
                entries: vec![bundle_entry(
                    "policy.md",
                    "---\ntitle: 旧政策\ntags: [old]\ncustom: 保留\n---\n\n# 旧政策\n\n条款。\n",
                )],
                namespace: None,
            },
            1000,
        )
        .await
        .unwrap();
    let doc_id = imported.docs[0].doc_id;
    let version_id = imported.docs[0].version_id;

    // Retitle via wiki_commit: the stored frontmatter block goes stale.
    let mut update = commit_input("新政策", "# 旧政策\n\n条款。\n");
    update.doc_id = Some(doc_id);
    update.parent_version = Some(version_id);
    update.tags = Some(vec!["new".to_string()]);
    wiki.commit("editor".to_string(), update, 2000)
        .await
        .unwrap();

    // Export updates known fields without dropping unknown business data.
    let export = wiki
        .export_bundle("e".to_string(), None, 3000)
        .await
        .unwrap();
    let entry = export
        .entries
        .iter()
        .find(|e| e.path == "policy.md")
        .unwrap();
    assert!(entry.content.contains("title: 新政策"), "{}", entry.content);
    let (raw, _) = okf::split_frontmatter(&entry.content);
    let fields: Json = serde_saphyr::from_str(raw.as_deref().unwrap()).unwrap();
    assert_eq!(fields["tags"], serde_json::json!(["new"]));
    assert!(
        entry.content.contains("custom: 保留"),
        "unknown fields must survive edits: {}",
        entry.content
    );
    assert!(
        !entry.content.contains("title: 旧政策"),
        "stale title line survived"
    );

    // Replay does not revert the document.
    let reimport = wiki
        .import_bundle(
            "i".to_string(),
            WikiImportInput {
                entries: export.entries,
                namespace: None,
            },
            4000,
        )
        .await
        .unwrap();
    assert_eq!(reimport.created, 0);
    let doc = wiki.get_doc(doc_id).await.unwrap();
    assert_eq!(doc.title, "新政策");
    assert_eq!(doc.tags, vec!["new".to_string()]);

    // The cycle converges: a second export/import round is a no-op.
    let export2 = wiki
        .export_bundle("e".to_string(), None, 5000)
        .await
        .unwrap();
    let reimport2 = wiki
        .import_bundle(
            "i".to_string(),
            WikiImportInput {
                entries: export2.entries,
                namespace: None,
            },
            6000,
        )
        .await
        .unwrap();
    assert_eq!(reimport2.unchanged, 1);
    assert_eq!(reimport2.created + reimport2.updated, 0);
}

#[tokio::test]
async fn okf_import_updates_changed_docs_in_place() {
    let wiki = test_wiki("wiki_okf_update").await;
    let v1 = WikiImportInput {
        entries: vec![bundle_entry("guide.md", "# 指南\n\n第一版内容。\n")],
        namespace: None,
    };
    let first = wiki.import_bundle("i".to_string(), v1, 1000).await.unwrap();
    assert_eq!(first.created, 1);

    let v2 = WikiImportInput {
        entries: vec![bundle_entry("guide.md", "# 指南\n\n第二版内容。\n")],
        namespace: None,
    };
    let second = wiki.import_bundle("i".to_string(), v2, 2000).await.unwrap();
    assert_eq!(second.updated, 1);
    assert_eq!(second.docs[0].doc_id, first.docs[0].doc_id);
    let versions = wiki
        .list_versions(first.docs[0].doc_id, None, Some(10))
        .await
        .unwrap();
    assert_eq!(versions.versions.len(), 2);
}

#[tokio::test]
async fn neighbor_expansion_widens_hits() {
    let wiki = test_wiki("wiki_expand").await;
    // Every section clears CHUNK_TARGET_MIN so the sibling-merge pass
    // keeps them as three separate chunks.
    let filler_a = "前置说明。".repeat(60);
    let filler_b = "核心细节。".repeat(60);
    let filler_c = "后续说明。".repeat(60);
    let content = format!(
        "# 邻域测试\n\n## 前言\n\n{filler_a}\n\n## 核心章节\n\n独特关键词：量子轨道谐振。\n\n{filler_b}\n\n## 附录\n\n{filler_c}\n"
    );
    let out = wiki
        .commit("u".to_string(), commit_input("邻域测试", &content), 1000)
        .await
        .unwrap();
    assert!(out.chunks >= 3, "fixture must span multiple chunks");

    // Baseline: the hit covers only the core section.
    let plain = wiki
        .search(WikiSearchInput::from_query("量子轨道谐振".to_string()))
        .await
        .unwrap();
    assert_eq!(plain.hits.len(), 1);
    assert!(!plain.hits[0].text.contains("前置说明"));

    // expand=1 pulls in both neighbors and the citation stays verifiable.
    let mut q = WikiSearchInput::from_query("量子轨道谐振".to_string());
    q.expand = Some(1);
    let expanded = wiki.search(q).await.unwrap();
    assert_eq!(expanded.hits.len(), 1);
    let hit = &expanded.hits[0];
    assert!(hit.text.contains("前置说明"));
    assert!(hit.text.contains("量子轨道谐振"));
    assert!(hit.text.contains("后续说明"));
    let (start, end) = hit.citation.byte_range;
    assert!(
        end - start > plain.hits[0].citation.byte_range.1 - plain.hits[0].citation.byte_range.0
    );
    let verified = wiki
        .verify(
            "u".to_string(),
            WikiVerifyInput {
                uri: Some(hit.citation.uri.clone()),
                checksum: Some(hit.citation.checksum.clone()),
                ..Default::default()
            },
            2000,
        )
        .await
        .unwrap();
    assert_eq!(verified.status, WikiVerifyStatus::Valid);

    // Adjacent hits expand independently: overlapping context may repeat
    // across hits, each with its own verifiable citation.
    let mut q = WikiSearchInput::from_query("前置说明 后续说明".to_string());
    q.expand = Some(2);
    q.top_k = Some(10);
    let both = wiki.search(q).await.unwrap();
    assert_eq!(both.hits.len(), 2);
    assert!(both.hits.iter().all(|h| h.text.contains("量子轨道谐振")));
}

#[tokio::test]
async fn okf_reimport_propagates_tag_deletion() {
    let wiki = test_wiki("wiki_okf_tag_delete").await;
    let with_tags = WikiImportInput {
        entries: vec![bundle_entry(
            "policy.md",
            "---\ntitle: 政策\ntags: [alpha, beta]\n---\n\n# 政策\n\n内容。\n",
        )],
        namespace: None,
    };
    let first = wiki
        .import_bundle("i".to_string(), with_tags, 1000)
        .await
        .unwrap();
    let doc_id = first.docs[0].doc_id;
    assert_eq!(
        wiki.get_doc(doc_id).await.unwrap().tags,
        vec!["alpha".to_string(), "beta".to_string()]
    );

    // The author deletes the `tags:` key: the deletion must propagate
    // instead of resurrecting the stored tags on the next export.
    let without_tags = WikiImportInput {
        entries: vec![bundle_entry(
            "policy.md",
            "---\ntitle: 政策\n---\n\n# 政策\n\n内容。\n",
        )],
        namespace: None,
    };
    let second = wiki
        .import_bundle("i".to_string(), without_tags, 2000)
        .await
        .unwrap();
    assert_eq!(second.updated, 1);
    assert!(wiki.get_doc(doc_id).await.unwrap().tags.is_empty());
    let export = wiki
        .export_bundle("e".to_string(), None, 3000)
        .await
        .unwrap();
    let entry = export
        .entries
        .iter()
        .find(|e| e.path == "policy.md")
        .unwrap();
    assert!(!entry.content.contains("tags:"), "{}", entry.content);
}

#[tokio::test]
async fn concurrent_imports_of_one_bundle_never_duplicate_docs() {
    let wiki = test_wiki("wiki_okf_concurrent").await;
    let input = WikiImportInput {
        entries: vec![bundle_entry("guide.md", "# 指南\n\n并发导入内容。\n")],
        namespace: None,
    };
    let w1 = wiki.clone();
    let w2 = wiki.clone();
    let (i1, i2) = (input.clone(), input.clone());
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { w1.import_bundle("甲".to_string(), i1, 1000).await }),
        tokio::spawn(async move { w2.import_bundle("乙".to_string(), i2, 1001).await }),
    );
    let (r1, r2) = (r1.unwrap().unwrap(), r2.unwrap().unwrap());
    // "Lookup + commit" is atomic under the write lock: exactly one
    // import creates; the other converges on the same document instead
    // of minting a suffixed duplicate.
    assert_eq!(r1.created + r2.created, 1);
    assert_eq!(r1.docs[0].doc_id, r2.docs[0].doc_id);
    let docs = wiki.list_docs(WikiListDocsInput::default()).await.unwrap();
    assert_eq!(docs.docs.len(), 1);
    assert_eq!(docs.docs[0].slug, "guide");
}
