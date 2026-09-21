use super::tests::{commit_input, test_wiki};
use super::*;

fn restricted(actor: &str, labels: &[&str]) -> WikiAccess {
    WikiAccess {
        actor: actor.to_string(),
        labels: Some(labels.iter().map(|s| s.to_string()).collect()),
    }
}

/// M4 acceptance: a probe document behind a label never reaches a
/// restricted caller — the ACL clause runs inside the same AndaDB query
/// as retrieval.
#[tokio::test]
async fn acl_probe_is_filtered_at_the_database_level() {
    let wiki = test_wiki("wiki_acl_probe").await;
    wiki.commit(
        "admin".to_string(),
        commit_input("公开手册", "# 公开手册\n\n公开的探针词：晨雾灯塔。\n"),
        1000,
    )
    .await
    .unwrap();
    let mut secret = commit_input("机密预案", "# 机密预案\n\n机密的探针词：夜航坐标。\n");
    secret.acl_label = Some("secret".to_string());
    let secret = wiki
        .commit("admin".to_string(), secret, 1100)
        .await
        .unwrap();
    assert_eq!(secret.doc.acl_label, "secret");

    // Unrestricted: both probes hit.
    let all = wiki
        .search(WikiSearchInput::from_query("夜航坐标".to_string()))
        .await
        .unwrap();
    assert_eq!(all.hits.len(), 1);

    // Restricted to a different label: the secret probe is invisible,
    // unlabeled content still hits.
    let outsider = restricted("outsider", &["public-team"]);
    let miss = wiki
        .search_scoped(
            &outsider,
            WikiSearchInput::from_query("夜航坐标".to_string()),
            2000,
        )
        .await
        .unwrap();
    assert!(miss.hits.is_empty());
    assert_eq!(miss.total_docs_matched, 0);
    let open_hit = wiki
        .search_scoped(
            &outsider,
            WikiSearchInput::from_query("晨雾灯塔".to_string()),
            2001,
        )
        .await
        .unwrap();
    assert_eq!(open_hit.hits.len(), 1);

    // Granted label: visible again.
    let insider = restricted("insider", &["secret"]);
    let hit = wiki
        .search_scoped(
            &insider,
            WikiSearchInput::from_query("夜航坐标".to_string()),
            2002,
        )
        .await
        .unwrap();
    assert_eq!(hit.hits.len(), 1);

    // Read/list/get/versions honor the same boundary; denial reads as
    // NotFound so existence does not leak.
    assert!(matches!(
        wiki.read_scoped(
            &outsider,
            WikiReadInput {
                doc_id: secret.doc.id,
                version: None,
                selector: WikiSelector::Full,
            },
            2003,
        )
        .await,
        Err(WikiError::NotFound(_))
    ));
    assert!(matches!(
        wiki.get_doc_scoped(&outsider, secret.doc.id).await,
        Err(WikiError::NotFound(_))
    ));
    assert!(matches!(
        wiki.list_versions_scoped(&outsider, secret.doc.id, None, None)
            .await,
        Err(WikiError::NotFound(_))
    ));
    let listed = wiki
        .list_docs_scoped(&outsider, WikiListDocsInput::default())
        .await
        .unwrap();
    assert!(listed.docs.iter().all(|d| d.acl_label.is_empty()));
    // verify: denial is indistinguishable from a nonexistent citation
    // (same 200 + not_found verdict) so it cannot enumerate hidden ids.
    let denied = wiki
        .verify_scoped(
            &outsider,
            WikiVerifyInput {
                uri: Some(citation_uri(
                    "test_space",
                    secret.doc.id,
                    secret.version.id,
                    0,
                    4,
                )),
                ..Default::default()
            },
            2004,
        )
        .await
        .unwrap();
    assert_eq!(denied.status, WikiVerifyStatus::NotFound);
    assert!(denied.current_version.is_none());
    assert!(denied.quote.is_none());
    let missing = wiki
        .verify_scoped(
            &outsider,
            WikiVerifyInput {
                uri: Some(citation_uri("test_space", 99999, 99999, 0, 4)),
                ..Default::default()
            },
            2005,
        )
        .await
        .unwrap();
    assert_eq!(missing.status, WikiVerifyStatus::NotFound);
}

#[tokio::test]
async fn agent_view_restricts_labeled_content_on_public_floor() {
    let wiki = test_wiki("wiki_agent_view").await;
    wiki.commit(
        "admin".to_string(),
        commit_input("公开文档", "# 公开文档\n\n公开探针：银杏大道。\n"),
        1000,
    )
    .await
    .unwrap();
    let mut secret = commit_input("受限文档", "# 受限文档\n\n受限探针：琥珀回廊。\n");
    secret.acl_label = Some("secret".to_string());
    let secret = wiki
        .commit("admin".to_string(), secret, 1100)
        .await
        .unwrap();

    // Private-space view (None): unrestricted.
    let all = wiki
        .search_view(WikiSearchInput::from_query("琥珀回廊".to_string()), None)
        .await
        .unwrap();
    assert_eq!(all.hits.len(), 1);

    // Public-space floor (Some([])): labeled content is invisible and
    // reads deny as NotFound.
    let floor: Vec<String> = Vec::new();
    let none = wiki
        .search_view(
            WikiSearchInput::from_query("琥珀回廊".to_string()),
            Some(&floor),
        )
        .await
        .unwrap();
    assert!(none.hits.is_empty());
    let open = wiki
        .search_view(
            WikiSearchInput::from_query("银杏大道".to_string()),
            Some(&floor),
        )
        .await
        .unwrap();
    assert_eq!(open.hits.len(), 1);
    assert!(matches!(
        wiki.read_view(
            WikiReadInput {
                doc_id: secret.doc.id,
                version: None,
                selector: WikiSelector::Full,
            },
            Some(&floor),
        )
        .await,
        Err(WikiError::NotFound(_))
    ));
}

#[tokio::test]
async fn acl_label_inherits_namespace_default_and_stays_idempotent() {
    let wiki = test_wiki("wiki_acl_defaults").await;
    wiki.set_acl_defaults(BTreeMap::from([(
        "hr".to_string(),
        "hr-internal".to_string(),
    )]))
    .await
    .unwrap();

    let mut input = commit_input("薪酬制度", "# 薪酬制度\n\n薪酬保密条款内容。\n");
    input.namespace = Some("hr".to_string());
    let out = wiki.commit("admin".to_string(), input, 1000).await.unwrap();
    assert_eq!(out.doc.acl_label, "hr-internal");

    // Same commit again (no acl_label supplied): idempotent, label kept.
    let mut again = commit_input("薪酬制度", "# 薪酬制度\n\n薪酬保密条款内容。\n");
    again.namespace = Some("hr".to_string());
    again.doc_id = Some(out.doc.id);
    again.parent_version = Some(out.version.id);
    let repeat = wiki.commit("admin".to_string(), again, 1100).await.unwrap();
    assert!(repeat.idempotent);

    // Explicit clear.
    let mut clear = commit_input("薪酬制度", "# 薪酬制度\n\n对外公开版本。\n");
    clear.doc_id = Some(out.doc.id);
    clear.parent_version = Some(out.version.id);
    clear.acl_label = Some(String::new());
    let cleared = wiki.commit("admin".to_string(), clear, 1200).await.unwrap();
    assert!(cleared.doc.acl_label.is_empty());
}

#[tokio::test]
async fn audit_reads_events_only_when_enabled() {
    let wiki = test_wiki("wiki_audit_reads").await;
    let out = wiki
        .commit(
            "admin".to_string(),
            commit_input("审计文档", "# 审计文档\n\n审计探针内容。\n"),
            1000,
        )
        .await
        .unwrap();
    let access = WikiAccess {
        actor: "auditor".to_string(),
        labels: None,
    };

    // Disabled (default): scoped reads leave no events.
    wiki.search_scoped(
        &access,
        WikiSearchInput::from_query("审计探针".to_string()),
        2000,
    )
    .await
    .unwrap();
    let events = wiki
        .list_events(Some(EVENT_WIKI_QUERIED.to_string()), None, None, Some(10))
        .await
        .unwrap();
    assert!(events.events.is_empty());
    assert_eq!(wiki.queries_count(), 1);

    // Enabled: search and read are evented with the real actor.
    wiki.set_audit_reads(true);
    wiki.search_scoped(
        &access,
        WikiSearchInput::from_query("审计探针".to_string()),
        2100,
    )
    .await
    .unwrap();
    wiki.read_scoped(
        &access,
        WikiReadInput {
            doc_id: out.doc.id,
            version: None,
            selector: WikiSelector::Toc,
        },
        2200,
    )
    .await
    .unwrap();
    let queried = wiki
        .list_events(Some(EVENT_WIKI_QUERIED.to_string()), None, None, Some(10))
        .await
        .unwrap();
    assert_eq!(queried.events.len(), 1);
    assert_eq!(queried.events[0].actor, "auditor");
    let read = wiki
        .list_events(Some(EVENT_WIKI_READ.to_string()), None, None, Some(10))
        .await
        .unwrap();
    assert_eq!(read.events.len(), 1);
    assert_eq!(wiki.queries_count(), 2);
}

#[tokio::test]
async fn event_prune_keeps_cap_and_stale_report_counts() {
    let wiki = test_wiki("wiki_housekeeping").await;
    for i in 0..6u64 {
        wiki.commit(
            "admin".to_string(),
            commit_input(
                &format!("文档{i}"),
                &format!("# 文档{i}\n\n内容编号 {i}。\n"),
            ),
            1000 + i,
        )
        .await
        .unwrap();
    }
    assert_eq!(wiki.events.metadata().stats.num_documents, 6);

    // Prune to 3: the three oldest go, the prune itself is evented.
    let removed = wiki.prune_events(3, 5000).await.unwrap();
    assert_eq!(removed, 3);
    let remaining = wiki.list_events(None, None, None, Some(50)).await.unwrap();
    assert_eq!(remaining.events.len(), 4); // 3 kept + EventsPruned
    assert!(
        remaining
            .events
            .iter()
            .any(|e| e.kind == EVENT_EVENTS_PRUNED)
    );

    // Stale report: everything is ancient relative to `now`.
    let report = wiki
        .stale_report(1000 + 365 * 24 * 3600 * 1000, DEFAULT_STALE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(report.stale_docs, 6);
    assert_eq!(report.checked_docs, 6);
    assert_eq!(wiki.stale_report_cached().stale_docs, 6);
    // Fresh threshold: nothing stale, cached report replaced.
    let report = wiki
        .stale_report(2000, DEFAULT_STALE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(report.stale_docs, 0);
    assert_eq!(wiki.stale_report_cached().stale_docs, 0);
}
