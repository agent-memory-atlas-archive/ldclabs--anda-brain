use super::*;
use anda_db::{database::DBConfig, storage::StorageConfig};
use object_store::memory::InMemory;

pub(super) async fn test_wiki(name: &str) -> WikiService {
    let db = Arc::new(
        AndaDB::create(
            Arc::new(InMemory::new()),
            DBConfig {
                name: name.to_string(),
                description: "wiki test db".to_string(),
                storage: StorageConfig::default(),
                lock: None,
            },
        )
        .await
        .unwrap(),
    );
    WikiService::connect("test_space".to_string(), db)
        .await
        .unwrap()
}

pub(super) fn commit_input(title: &str, content: &str) -> WikiCommitInput {
    WikiCommitInput {
        title: title.to_string(),
        content: content.to_string(),
        ..Default::default()
    }
}

const CN_DOC: &str = "# 部署指南\n\n本指南描述生产环境的部署步骤与回滚策略。\n\n## 前置条件\n\n需要配置对象存储与访问令牌，并确认分片参数一致。\n\n```bash\n# 这行注释不是标题\nexport ANDA_TOKEN=secret\n```\n\n## 回滚策略\n\n出现故障时使用上一版本快照回滚，并验证引用校验和。\n";

#[tokio::test]
async fn commit_search_read_verify_roundtrip() {
    let wiki = test_wiki("wiki_roundtrip").await;
    let out = wiki
        .commit(
            "user:alice".to_string(),
            commit_input("部署指南", CN_DOC),
            1000,
        )
        .await
        .unwrap();
    assert!(out.created);
    assert!(!out.idempotent);
    assert!(out.chunks >= 1);
    assert_eq!(out.doc.created_by, "user:alice");
    assert_eq!(out.version.author, "user:alice");

    // Search hits with precise citations.
    let rt = wiki
        .search(WikiSearchInput::from_query("回滚策略".to_string()))
        .await
        .unwrap();
    assert!(!rt.hits.is_empty());
    let hit = &rt.hits[0];
    assert_eq!(hit.doc_title, "部署指南");
    assert!(hit.citation.uri.starts_with("wiki://test_space/"));

    // The citation byte range slices the normalized content exactly.
    let read = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Full,
        })
        .await
        .unwrap();
    let content = read.content.unwrap();
    let (start, end) = hit.citation.byte_range;
    assert_eq!(&content[start as usize..end as usize], hit.text);
    // The code-fence comment must not become a heading/section.
    assert!(
        !hit.citation
            .heading_path
            .iter()
            .any(|h| h.contains("这行注释"))
    );

    // TOC and section reads.
    let toc = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Toc,
        })
        .await
        .unwrap()
        .toc
        .unwrap();
    assert!(!toc.is_empty());
    let anchor = &hit.citation.anchor;
    let section = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Section {
                anchor: anchor.clone(),
            },
        })
        .await
        .unwrap();
    assert!(section.content.unwrap().contains(&hit.text));

    // Verify: valid via uri + checksum.
    let verified = wiki
        .verify(
            "user:alice".to_string(),
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
}

#[tokio::test]
async fn idempotent_commits_do_not_grow_versions() {
    let wiki = test_wiki("wiki_idempotent").await;
    let first = wiki
        .commit(
            "a".to_string(),
            commit_input("政策", "# 政策\n\n条款内容。\n"),
            1000,
        )
        .await
        .unwrap();

    for i in 0..100u64 {
        let mut input = commit_input("政策", "# 政策\n\n条款内容。\n");
        input.doc_id = Some(first.doc.id);
        input.parent_version = Some(first.version.id);
        let out = wiki.commit("a".to_string(), input, 2000 + i).await.unwrap();
        assert!(out.idempotent);
        assert_eq!(out.version.id, first.version.id);
    }

    let versions = wiki
        .list_versions(first.doc.id, None, Some(100))
        .await
        .unwrap();
    assert_eq!(versions.versions.len(), 1);
    assert_eq!(wiki.chunks_count(), first.chunks);
}

#[tokio::test]
async fn cas_conflict_version_chain_and_chunk_replacement() {
    let wiki = test_wiki("wiki_cas").await;
    let v1 = wiki
        .commit(
            "a".to_string(),
            commit_input("手册", "# 手册\n\n第一版内容：旧的错误码说明。\n"),
            1000,
        )
        .await
        .unwrap();
    let chunks_after_v1 = wiki.chunks_count();

    // Update without parent_version fails.
    let mut missing = commit_input("手册", "# 手册\n\n新内容。\n");
    missing.doc_id = Some(v1.doc.id);
    assert!(matches!(
        wiki.commit("b".to_string(), missing, 1500).await,
        Err(WikiError::Invalid(_))
    ));

    // Correct CAS succeeds and links the version chain.
    let mut update = commit_input("手册", "# 手册\n\n第二版内容：新的重试策略说明。\n");
    update.doc_id = Some(v1.doc.id);
    update.parent_version = Some(v1.version.id);
    let v2 = wiki.commit("b".to_string(), update, 2000).await.unwrap();
    assert!(!v2.created);
    assert_eq!(v2.version.parent_version, Some(v1.version.id));

    // Stale CAS now conflicts, reporting the current version.
    let mut stale = commit_input("手册", "# 手册\n\n第三版内容。\n");
    stale.doc_id = Some(v1.doc.id);
    stale.parent_version = Some(v1.version.id);
    match wiki.commit("c".to_string(), stale, 3000).await {
        Err(WikiError::Conflict {
            current_version,
            updated_by,
            ..
        }) => {
            assert_eq!(current_version, v2.version.id);
            assert_eq!(updated_by, "b");
        }
        other => panic!("expected conflict, got {other:?}"),
    }

    // Old chunks replaced, not accumulated; search sees only v2.
    assert_eq!(wiki.chunks_count(), chunks_after_v1 - v1.chunks + v2.chunks);
    let rt = wiki
        .search(WikiSearchInput::from_query("错误码".to_string()))
        .await
        .unwrap();
    assert!(rt.hits.is_empty());
    let rt = wiki
        .search(WikiSearchInput::from_query("重试策略".to_string()))
        .await
        .unwrap();
    assert_eq!(rt.hits[0].citation.version_id, v2.version.id);

    // Historical version still readable (re-chunked layout) and verify
    // reports it superseded.
    let old = wiki
        .read(WikiReadInput {
            doc_id: v1.doc.id,
            version: Some(v1.version.id),
            selector: WikiSelector::Toc,
        })
        .await
        .unwrap();
    assert!(!old.is_current);
    assert!(old.toc.unwrap().iter().any(|t| !t.anchor.is_empty()));
    let verified = wiki
        .verify(
            "a".to_string(),
            WikiVerifyInput {
                doc_id: Some(v1.doc.id),
                version_id: Some(v1.version.id),
                byte_range: Some((0, 2)),
                ..Default::default()
            },
            4000,
        )
        .await
        .unwrap();
    assert_eq!(verified.status, WikiVerifyStatus::Superseded);
    assert_eq!(verified.current_version, Some(v2.version.id));
}

#[tokio::test]
async fn chinese_titles_never_collide() {
    let wiki = test_wiki("wiki_slug_cn").await;
    let a = wiki
        .commit(
            "a".to_string(),
            commit_input("产品手册", "# 产品手册\n\n产品功能介绍。\n"),
            1000,
        )
        .await
        .unwrap();
    let b = wiki
        .commit(
            "a".to_string(),
            commit_input("安全政策", "# 安全政策\n\n安全合规要求。\n"),
            1100,
        )
        .await
        .unwrap();
    // v1 regression: both would slugify to "untitled" and silently merge.
    assert_ne!(a.doc.id, b.doc.id);
    assert_ne!(a.doc.slug, b.doc.slug);
    assert!(a.doc.slug.contains("产品手册"));

    // Same title twice: suffixing, never merging.
    let c = wiki
        .commit(
            "a".to_string(),
            commit_input("产品手册", "# 产品手册\n\n另一篇同名文档。\n"),
            1200,
        )
        .await
        .unwrap();
    assert_ne!(c.doc.id, a.doc.id);
    assert_ne!(c.doc.slug, a.doc.slug);

    let docs = wiki.list_docs(WikiListDocsInput::default()).await.unwrap();
    assert_eq!(docs.docs.len(), 3);
}

#[tokio::test]
async fn archive_hides_from_search_and_restore_recovers() {
    let wiki = test_wiki("wiki_archive").await;
    let out = wiki
        .commit(
            "a".to_string(),
            commit_input("旧规范", "# 旧规范\n\n历史合规要求文本。\n"),
            1000,
        )
        .await
        .unwrap();

    let archived = wiki
        .archive("b".to_string(), out.doc.id, 2000)
        .await
        .unwrap();
    assert_eq!(archived.status, DOC_STATUS_ARCHIVED);
    let rt = wiki
        .search(WikiSearchInput::from_query("合规要求".to_string()))
        .await
        .unwrap();
    assert!(rt.hits.is_empty());

    // Still readable by id; commit to archived doc rejected.
    let read = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Full,
        })
        .await
        .unwrap();
    assert!(read.content.unwrap().contains("历史合规要求"));
    let mut update = commit_input("旧规范", "# 旧规范\n\n修改。\n");
    update.doc_id = Some(out.doc.id);
    update.parent_version = Some(out.version.id);
    assert!(matches!(
        wiki.commit("b".to_string(), update, 2500).await,
        Err(WikiError::Invalid(_))
    ));

    wiki.restore("b".to_string(), out.doc.id, 3000)
        .await
        .unwrap();
    let rt = wiki
        .search(WikiSearchInput::from_query("合规要求".to_string()))
        .await
        .unwrap();
    assert_eq!(rt.hits.len(), 1);

    // Audit trail recorded real actors.
    let events = wiki
        .list_events(None, Some(out.doc.id), None, Some(10))
        .await
        .unwrap();
    let kinds: Vec<_> = events.events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&EVENT_DOC_CREATED));
    assert!(kinds.contains(&EVENT_DOC_ARCHIVED));
    assert!(kinds.contains(&EVENT_DOC_RESTORED));
    assert!(
        events
            .events
            .iter()
            .all(|e| e.actor == "a" || e.actor == "b")
    );
}

#[tokio::test]
async fn size_and_validity_limits() {
    let wiki = test_wiki("wiki_limits").await;
    let huge = "字".repeat(MAX_DOC_BYTES / 3 + 1);
    assert!(matches!(
        wiki.commit("a".to_string(), commit_input("大文档", &huge), 1000)
            .await,
        Err(WikiError::TooLarge { .. })
    ));
    assert!(matches!(
        wiki.commit("a".to_string(), commit_input("空", "   \n  \n"), 1000)
            .await,
        Err(WikiError::Invalid(_))
    ));
    assert!(matches!(
        wiki.commit(
            "a".to_string(),
            commit_input("", "no heading content"),
            1000
        )
        .await,
        Err(WikiError::Invalid(_))
    ));
    // Tag caps are enforced on the write path.
    let mut tagged = commit_input("标签", "# 标签\n\n内容。\n");
    tagged.tags = Some((0..MAX_TAGS + 1).map(|i| format!("t{i}")).collect());
    assert!(matches!(
        wiki.commit("a".to_string(), tagged, 1000).await,
        Err(WikiError::Invalid(_))
    ));
}

#[tokio::test]
async fn concurrent_updates_exactly_one_wins() {
    let wiki = test_wiki("wiki_concurrent").await;
    let v1 = wiki
        .commit(
            "a".to_string(),
            commit_input("竞争文档", "# 竞争文档\n\n初始内容。\n"),
            1000,
        )
        .await
        .unwrap();

    let mk = |text: &str| {
        let mut input = commit_input("竞争文档", text);
        input.doc_id = Some(v1.doc.id);
        input.parent_version = Some(v1.version.id);
        input
    };
    let w1 = wiki.clone();
    let w2 = wiki.clone();
    let i1 = mk("# 竞争文档\n\n写者甲的修改。\n");
    let i2 = mk("# 竞争文档\n\n写者乙的修改。\n");
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { w1.commit("甲".to_string(), i1, 2000).await }),
        tokio::spawn(async move { w2.commit("乙".to_string(), i2, 2001).await }),
    );
    let results = [r1.unwrap(), r2.unwrap()];
    let oks = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(WikiError::Conflict { .. })))
        .count();
    assert_eq!((oks, conflicts), (1, 1));
}

#[tokio::test]
async fn orphan_sweep_reclaims_crash_leftovers() {
    let wiki = test_wiki("wiki_sweep").await;
    let out = wiki
        .commit(
            "a".to_string(),
            commit_input("正常文档", "# 正常文档\n\n正常内容。\n"),
            1000,
        )
        .await
        .unwrap();

    // Simulate a crash between the version write and the doc flip: an
    // orphan version with inactive chunks.
    let orphan_version = wiki
        .versions
        .add_from(&WikiVersionRecord {
            _id: 0,
            doc_id: out.doc.id,
            parent_version: Some(out.version.id),
            checksum: "sha3-256:dead".to_string(),
            content: "# 孤儿版本\n".to_string(),
            size: 12,
            author: "a".to_string(),
            message: None,
            created_at: 2000,
        })
        .await
        .unwrap();
    wiki.chunks
        .add_from(&WikiChunkRecord {
            _id: 0,
            doc_id: out.doc.id,
            version_id: orphan_version,
            namespace: "default".to_string(),
            current: 0,
            title: "正常文档".to_string(),
            heading_path: vec![],
            anchor: "section-0".to_string(),
            ordinal: 0,
            text: "孤儿".to_string(),
            byte_start: 0,
            byte_end: 6,
            checksum: "sha3-256:dead".to_string(),
            chunker_version: CHUNKER_VERSION as u64,
            acl_label: String::new(),
        })
        .await
        .unwrap();

    // Simulate a crashed create: a sentinel doc past the TTL.
    wiki.docs
        .add_from(&WikiDocRecord {
            generation: 1,
            digest_pending: 0,
            _id: 0,
            namespace: "default".to_string(),
            slug: "sentinel".to_string(),
            title: "半成品".to_string(),
            status: DOC_STATUS_ACTIVE.to_string(),
            current_version: 0,
            current_checksum: String::new(),
            tags: vec![],
            acl_label: String::new(),
            source_uri: None,
            metadata: BTreeMap::new(),
            created_by: "a".to_string(),
            updated_by: "a".to_string(),
            created_at: 1000,
            updated_at: 1000,
        })
        .await
        .unwrap();

    // Orphans are invisible before the sweep.
    let versions = wiki
        .list_versions(out.doc.id, None, Some(50))
        .await
        .unwrap();
    assert_eq!(versions.versions.len(), 1);
    let docs = wiki.list_docs(WikiListDocsInput::default()).await.unwrap();
    assert_eq!(docs.docs.len(), 1);

    let report = wiki.orphan_sweep(1000 + SENTINEL_TTL_MS + 1).await.unwrap();
    assert_eq!(report.docs_removed, 1);
    assert_eq!(report.versions_removed, 1);
    assert_eq!(report.chunks_removed, 1);

    // The healthy document is untouched and searchable.
    let rt = wiki
        .search(WikiSearchInput::from_query("正常内容".to_string()))
        .await
        .unwrap();
    assert_eq!(rt.hits.len(), 1);
    let report = wiki.orphan_sweep(1000 + SENTINEL_TTL_MS + 2).await.unwrap();
    assert!(report.is_empty());
}

#[tokio::test]
async fn search_filters_and_docs_mode() {
    let wiki = test_wiki("wiki_filters").await;
    let mut a = commit_input(
        "检索文档甲",
        "# 检索文档甲\n\n共享关键词：分布式检索测试。\n",
    );
    a.namespace = Some("engineering".to_string());
    a.tags = Some(vec!["api".to_string()]);
    let a = wiki.commit("u".to_string(), a, 1000).await.unwrap();

    let mut b = commit_input(
        "检索文档乙",
        "# 检索文档乙\n\n共享关键词：分布式检索测试。\n\n## 附录\n\n共享关键词：分布式检索测试补充。\n",
    );
    b.namespace = Some("policy".to_string());
    b.tags = Some(vec!["compliance".to_string()]);
    let b = wiki.commit("u".to_string(), b, 1100).await.unwrap();

    // Namespace filter.
    let mut q = WikiSearchInput::from_query("分布式检索测试".to_string());
    q.namespaces = vec!["engineering".to_string()];
    let rt = wiki.search(q).await.unwrap();
    assert!(!rt.hits.is_empty());
    assert!(rt.hits.iter().all(|h| h.citation.doc_id == a.doc.id));

    // Tag filter.
    let mut q = WikiSearchInput::from_query("分布式检索测试".to_string());
    q.tags = vec!["compliance".to_string()];
    let rt = wiki.search(q).await.unwrap();
    assert!(!rt.hits.is_empty());
    assert!(rt.hits.iter().all(|h| h.citation.doc_id == b.doc.id));

    // Docs mode dedupes to one hit per document.
    let mut q = WikiSearchInput::from_query("分布式检索测试".to_string());
    q.mode = WikiSearchMode::Docs;
    q.top_k = Some(10);
    let rt = wiki.search(q).await.unwrap();
    let doc_ids: Vec<_> = rt.hits.iter().map(|h| h.citation.doc_id).collect();
    let unique: std::collections::BTreeSet<_> = doc_ids.iter().collect();
    assert_eq!(doc_ids.len(), unique.len());
    assert_eq!(rt.total_docs_matched, 2);
}

#[tokio::test]
async fn range_read_is_bounded_and_conflict_carries_checksum() {
    let wiki = test_wiki("wiki_range_bound").await;
    let out = wiki
        .commit(
            "a".to_string(),
            commit_input("边界文档", "# 边界文档\n\n正文内容若干。\n"),
            1000,
        )
        .await
        .unwrap();

    // Range{0, u64::MAX} is clamped to MAX_READ_BYTES semantics (here the
    // doc is small, so it reads fully but must not error).
    let read = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Range {
                start: 0,
                end: u64::MAX,
            },
        })
        .await
        .unwrap();
    assert!(!read.truncated);
    assert_eq!(read.content.unwrap().len() as u64, out.version.size);

    // Conflict reports the current checksum for read-merge-retry.
    let mut stale = commit_input("边界文档", "# 边界文档\n\n改动。\n");
    stale.doc_id = Some(out.doc.id);
    stale.parent_version = Some(out.version.id + 999);
    match wiki.commit("b".to_string(), stale, 2000).await {
        Err(WikiError::Conflict {
            current_version,
            current_checksum,
            ..
        }) => {
            assert_eq!(current_version, out.version.id);
            assert_eq!(current_checksum, out.doc.current_checksum);
        }
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn reserved_okf_slugs_are_suffixed() {
    let wiki = test_wiki("wiki_reserved_slug").await;
    let a = wiki
        .commit(
            "a".to_string(),
            commit_input("Index", "# Index\n\n首页文档。\n"),
            1000,
        )
        .await
        .unwrap();
    assert_ne!(a.doc.slug, "index");
    let mut b = commit_input("日志", "# 日志\n\n记录。\n");
    b.slug = Some("notes/log".to_string());
    let b = wiki.commit("a".to_string(), b, 1100).await.unwrap();
    assert_ne!(b.doc.slug, "notes/log");
    assert!(b.doc.slug.starts_with("notes/log-"));
}

#[tokio::test]
async fn eval_namespace_is_excluded_from_default_search() {
    let wiki = test_wiki("wiki_eval_excluded").await;
    let mut eval_doc = commit_input("评测文档", "# 评测文档\n\n评测探针：孤峰栈道。\n");
    eval_doc.namespace = Some(EVAL_NAMESPACE.to_string());
    wiki.commit("eval".to_string(), eval_doc, 1000)
        .await
        .unwrap();

    let default = wiki
        .search(WikiSearchInput::from_query("孤峰栈道".to_string()))
        .await
        .unwrap();
    assert!(default.hits.is_empty());

    let mut explicit = WikiSearchInput::from_query("孤峰栈道".to_string());
    explicit.namespaces = vec![EVAL_NAMESPACE.to_string()];
    let explicit = wiki.search(explicit).await.unwrap();
    assert_eq!(explicit.hits.len(), 1);
}

#[tokio::test]
async fn orphan_sweep_reconciles_past_the_single_query_cap() {
    let wiki = test_wiki("wiki_sweep_paged").await;
    let out = wiki
        .commit(
            "a".to_string(),
            commit_input("大文档", "# 大文档\n\n当前版本内容。\n"),
            1000,
        )
        .await
        .unwrap();

    // Simulate a crash between activation (step 4) and cleanup (step 5)
    // with a stale chunk set larger than one search page: an
    // unpaginated reconciliation would leave the overflow visible.
    let stale_total = Collection::MAX_SEARCH_LIMIT + 5;
    for i in 0..stale_total {
        wiki.chunks
            .add_from(&WikiChunkRecord {
                _id: 0,
                doc_id: out.doc.id,
                version_id: out.version.id + 999,
                namespace: "default".to_string(),
                current: 1,
                title: "大文档".to_string(),
                heading_path: vec![],
                anchor: format!("stale-{i}"),
                ordinal: i as u64,
                text: "过期残留".to_string(),
                byte_start: 0,
                byte_end: 12,
                checksum: "sha3-256:stale".to_string(),
                chunker_version: CHUNKER_VERSION as u64,
                acl_label: String::new(),
            })
            .await
            .unwrap();
    }

    let report = wiki.orphan_sweep(2000).await.unwrap();
    assert_eq!(report.chunks_removed, stale_total);
    assert_eq!(
        wiki.all_chunk_ids_of(out.doc.id).await.unwrap().len(),
        out.chunks
    );
    let rt = wiki
        .search(WikiSearchInput::from_query("过期残留".to_string()))
        .await
        .unwrap();
    assert!(rt.hits.is_empty());
}

#[tokio::test]
async fn section_read_survives_corrupt_chunk_ranges() {
    let wiki = test_wiki("wiki_section_corrupt").await;
    let out = wiki
        .commit(
            "a".to_string(),
            commit_input("切片文档", "# 切片文档\n\n第一段正文。\n"),
            1000,
        )
        .await
        .unwrap();
    let toc = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Toc,
        })
        .await
        .unwrap()
        .toc
        .unwrap();
    let anchor = toc[0].anchor.clone();

    // Retrieval corruption must not affect source-derived section reads.
    for id in wiki.all_chunk_ids_of(out.doc.id).await.unwrap() {
        wiki.chunks
            .update(
                id,
                BTreeMap::from([("byte_end".to_string(), Fv::U64(10_000))]),
            )
            .await
            .unwrap();
    }
    let read = wiki
        .read(WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Section { anchor },
        })
        .await
        .unwrap();
    assert_eq!(
        read.content.as_deref(),
        Some("# 切片文档\n\n第一段正文。\n")
    );
}

#[tokio::test]
async fn prune_keeps_only_newest_digest_ledger_head_per_doc() {
    let wiki = test_wiki("wiki_prune_ledger").await;
    let doc = wiki
        .commit(
            "a".to_string(),
            commit_input("账本文档", "# 账本文档\n\n内容。\n"),
            1000,
        )
        .await
        .unwrap();
    let old_digest = wiki
        .write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(doc.doc.id),
            Some(doc.version.id),
            "wiki_digest".to_string(),
            BTreeMap::new(),
            1100,
        )
        .await
        .unwrap();
    let head_digest = wiki
        .write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(doc.doc.id),
            Some(doc.version.id),
            "wiki_digest".to_string(),
            BTreeMap::new(),
            1200,
        )
        .await
        .unwrap();
    for i in 0..4u64 {
        wiki.commit(
            "a".to_string(),
            commit_input(&format!("填充{i}"), &format!("# 填充{i}\n\n内容 {i}。\n")),
            1300 + i,
        )
        .await
        .unwrap();
    }

    let removed = wiki.prune_events(1, 5000).await.unwrap();
    assert!(removed > 0);
    let ids: Vec<u64> = wiki
        .list_events(None, None, None, Some(50))
        .await
        .unwrap()
        .events
        .iter()
        .map(|e| e.id)
        .collect();
    assert!(ids.contains(&head_digest), "ledger head must survive");
    assert!(!ids.contains(&old_digest), "old digest rows are prunable");
}

#[tokio::test]
async fn list_docs_paginates_without_gaps_or_dups() {
    let wiki = test_wiki("wiki_paging").await;
    for i in 0..5 {
        wiki.commit(
            "u".to_string(),
            commit_input(
                &format!("分页文档{i}"),
                &format!("# 分页文档{i}\n\n内容 {i}。\n"),
            ),
            1000 + i,
        )
        .await
        .unwrap();
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = wiki
            .list_docs(WikiListDocsInput {
                cursor: cursor.clone(),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        for doc in &page.docs {
            assert!(seen.insert(doc.id), "duplicate doc {} across pages", doc.id);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
}
