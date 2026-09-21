use super::*;

// Digest lifecycle regressions with deterministic model outputs.
#[derive(Debug)]
struct ReviewCompleter(std::sync::Mutex<std::collections::VecDeque<String>>);

impl anda_engine::model::CompletionFeaturesDyn for ReviewCompleter {
    fn model_name(&self) -> String {
        "wiki-review-model".into()
    }

    fn completion(
        &self,
        _request: anda_core::CompletionRequest,
    ) -> anda_core::BoxPinFut<Result<anda_core::AgentOutput, BoxError>> {
        let content = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call");
        Box::pin(async move {
            Ok(anda_core::AgentOutput {
                content,
                ..Default::default()
            })
        })
    }
}

async fn review_space(name: &str, replies: Vec<String>) -> Arc<crate::space::Space> {
    let models = crate::testkit::models_with_completer(ReviewCompleter(std::sync::Mutex::new(
        replies.into(),
    )));
    let app = crate::testkit::app_state_core(name, models, vec![], "review", 0);
    let space = crate::testkit::create_loaded_space(&app, name).await;
    space.db.set_extension_from("wiki_digest".into(), true);
    space
}

fn review_reply() -> String {
    json!({"facts": [{
        "subject": {"type": "Person", "name": "alice"},
        "predicate": "prefers",
        "object": {"type": "Preference", "name": "dark_mode"},
        "confidence": 0.9
    }]})
    .to_string()
}

#[tokio::test]
async fn archive_withdraws_already_digested_claim() {
    let space = review_space("review_archive", vec![review_reply()]).await;
    let created = space
        .wiki
        .commit(
            "a".into(),
            commit_input("Preference", "# Preference\nAlice prefers dark_mode.\n"),
            1000,
        )
        .await
        .unwrap();
    let first = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(first.digested, 1);
    space
        .wiki
        .archive("a".into(), created.doc.id, 2000)
        .await
        .unwrap();
    let after = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    let item = fact(("Person", "alice"), "prefers", ("Preference", "dark_mode"));
    let states = digest_claim_status(&space.wiki_digest, &item).await;
    assert_eq!(states, ["retracted"], "archive digest report: {after:?}");
}

#[tokio::test]
async fn partial_extraction_does_not_withdraw_unchanged_fact() {
    let space = review_space("review_omission", vec![review_reply()]).await;
    let content = "# Preference\nAlice prefers dark_mode.\n";
    let created = space
        .wiki
        .commit("a".into(), commit_input("Preference", content), 1000)
        .await
        .unwrap();
    space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    let mut update = commit_input("Preference (renamed)", content);
    update.doc_id = Some(created.doc.id);
    update.parent_version = Some(created.version.id);
    space.wiki.commit("a".into(), update, 2000).await.unwrap();
    let after = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    let item = fact(("Person", "alice"), "prefers", ("Preference", "dark_mode"));
    assert_eq!(
        digest_claim_status(&space.wiki_digest, &item).await,
        ["active"],
        "unchanged document body, digest report: {after:?}"
    );
}

#[tokio::test]
async fn omitted_claim_stays_in_ledger_and_can_later_be_withdrawn() {
    let space = review_space(
        "digest_omission",
        vec![review_reply(), json!({"facts": []}).to_string()],
    )
    .await;
    let first = space
        .wiki
        .commit(
            "a".into(),
            commit_input("Preference", "Alice prefers dark_mode.\n"),
            1000,
        )
        .await
        .unwrap();
    space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    let mut input = commit_input(
        "Preference",
        "Alice prefers dark_mode.\nAdditional explanation.\n",
    );
    input.doc_id = Some(first.doc.id);
    input.parent_version = Some(first.version.id);
    space.wiki.commit("a".into(), input, 2000).await.unwrap();
    let report = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(report.superseded, 0);
    assert_eq!(report.citations_invalid, 0);
    let item = fact(("Person", "alice"), "prefers", ("Preference", "dark_mode"));
    assert_eq!(
        digest_claim_status(&space.wiki_digest, &item).await,
        ["active"]
    );
    space
        .wiki
        .archive("a".into(), first.doc.id, 3000)
        .await
        .unwrap();
    let report = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(report.superseded, 1);
    assert_eq!(
        digest_claim_status(&space.wiki_digest, &item).await,
        ["retracted"]
    );
}

#[tokio::test]
async fn withdrawal_requires_an_explicit_absent_review_from_every_batch() {
    let absent = json!({"facts": [], "reviews": [{"index": 0, "verdict": "absent"}]}).to_string();
    let unknown = json!({"facts": [], "reviews": [{"index": 0, "verdict": "unknown"}]}).to_string();
    let space = review_space(
        "digest_coverage",
        vec![
            review_reply(),
            absent.clone(),
            unknown,
            absent.clone(),
            absent,
        ],
    )
    .await;
    let first = space
        .wiki
        .commit(
            "a".into(),
            commit_input("Preference", "Alice prefers dark_mode.\n"),
            1000,
        )
        .await
        .unwrap();
    space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    let body = format!("# Reference\n{}", "Reference information.\n\n".repeat(1300));
    let mut update = commit_input("Reference", &body);
    update.doc_id = Some(first.doc.id);
    update.parent_version = Some(first.version.id);
    let second = space.wiki.commit("a".into(), update, 2000).await.unwrap();
    let unknown = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(unknown.failed, 0);
    assert_eq!(unknown.superseded, 0);
    let item = fact(("Person", "alice"), "prefers", ("Preference", "dark_mode"));
    assert_eq!(
        digest_claim_status(&space.wiki_digest, &item).await,
        ["active"]
    );
    let mut update = commit_input("Reference", &format!("{body}End.\n"));
    update.doc_id = Some(first.doc.id);
    update.parent_version = Some(second.version.id);
    space.wiki.commit("a".into(), update, 3000).await.unwrap();
    let checked = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(checked.failed, 0);
    assert_eq!(checked.superseded, 1);
    assert_eq!(
        digest_claim_status(&space.wiki_digest, &item).await,
        ["retracted"]
    );
}

#[tokio::test]
async fn failed_extraction_stays_pending_without_blocking_other_documents() {
    let space = review_space(
        "digest_retry",
        vec![
            "invalid".into(),
            "invalid".into(),
            "{\"facts\":[]}".into(),
            review_reply(),
        ],
    )
    .await;
    let first = space
        .wiki
        .commit(
            "a".into(),
            commit_input("First", "Alice prefers dark_mode.\n"),
            1000,
        )
        .await
        .unwrap();
    let second = space
        .wiki
        .commit(
            "a".into(),
            commit_input("Second", "An introduction.\n"),
            2000,
        )
        .await
        .unwrap();
    let report = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.digested, 1);
    assert_eq!(
        space
            .wiki
            .doc_record(first.doc.id)
            .await
            .unwrap()
            .digest_pending,
        1
    );
    assert_eq!(
        space
            .wiki
            .doc_record(second.doc.id)
            .await
            .unwrap()
            .digest_pending,
        0
    );
    let report = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(report.failed, 0);
    assert_eq!(report.digested, 1);
    assert_eq!(
        space
            .wiki
            .doc_record(first.doc.id)
            .await
            .unwrap()
            .digest_pending,
        0
    );
}

#[derive(Debug)]
struct BlockingExtractor {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl anda_engine::model::CompletionFeaturesDyn for BlockingExtractor {
    fn model_name(&self) -> String {
        "blocking-digest".into()
    }
    fn completion(
        &self,
        _: CompletionRequest,
    ) -> anda_core::BoxPinFut<Result<anda_core::AgentOutput, BoxError>> {
        let started = self.started.clone();
        let release = self.release.clone();
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            Ok(anda_core::AgentOutput {
                content: review_reply(),
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn acl_change_during_extraction_fences_the_graph_write() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let models = crate::testkit::models_with_completer(BlockingExtractor {
        started: started.clone(),
        release: release.clone(),
    });
    let app = crate::testkit::app_state_core("digest_fence", models, vec![], "test", 0);
    let space = crate::testkit::create_loaded_space(&app, "digest_fence").await;
    space.db.set_extension_from("wiki_digest".into(), true);
    let first = space
        .wiki
        .commit(
            "a".into(),
            commit_input("First", "Alice prefers dark_mode.\n"),
            1000,
        )
        .await
        .unwrap();
    let running = space.clone();
    let digest =
        tokio::spawn(async move { running.run_wiki_digest(crate::agents::SELF_USER_ID).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let mut update = commit_input("First", "Private text.\n");
    update.doc_id = Some(first.doc.id);
    update.parent_version = Some(first.version.id);
    update.acl_label = Some("secret".into());
    space.wiki.commit("a".into(), update, 2000).await.unwrap();
    release.notify_one();
    let report = digest.await.unwrap().unwrap();
    assert_eq!(report.digested, 0);
    assert_eq!(
        space
            .wiki
            .doc_record(first.doc.id)
            .await
            .unwrap()
            .digest_pending,
        1
    );
    let item = fact(("Person", "alice"), "prefers", ("Preference", "dark_mode"));
    assert!(
        digest_claim_status(&space.wiki_digest, &item)
            .await
            .is_empty()
    );
    let report = space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(report.skipped, 1);
    assert_eq!(
        space
            .wiki
            .doc_record(first.doc.id)
            .await
            .unwrap()
            .digest_pending,
        0
    );
}

#[tokio::test]
async fn archived_digest_work_survives_space_close_and_reopen() {
    let store = Arc::new(object_store::memory::InMemory::new());
    let models = crate::testkit::models_with_completer(ReviewCompleter(std::sync::Mutex::new(
        vec![review_reply()].into(),
    )));
    let app = crate::testkit::app_state_core("digest_restart", models, vec![], "test", 0)
        .fork_with_store(store.clone());
    let space = crate::testkit::create_loaded_space(&app, "digest_restart").await;
    space.db.set_extension_from("wiki_digest".into(), true);
    let first = space
        .wiki
        .commit(
            "a".into(),
            commit_input("Preference", "Alice prefers dark_mode.\n"),
            1000,
        )
        .await
        .unwrap();
    space
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    space
        .wiki
        .archive("a".into(), first.doc.id, 2000)
        .await
        .unwrap();
    space.close().await.unwrap();
    let reopened = app
        .fork_with_store(store)
        .load_space_with("digest_restart", false, false)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .wiki
            .doc_record(first.doc.id)
            .await
            .unwrap()
            .digest_pending,
        1
    );
    let report = reopened
        .run_wiki_digest(crate::agents::SELF_USER_ID)
        .await
        .unwrap();
    assert_eq!(report.superseded, 1);
    assert_eq!(
        reopened
            .wiki
            .doc_record(first.doc.id)
            .await
            .unwrap()
            .digest_pending,
        0
    );
    reopened.close().await.unwrap();
}
