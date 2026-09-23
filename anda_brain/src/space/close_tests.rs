//! Crash-boundary close tests. The injected PUT is durable before its future
//! hangs, so cancelling it really poisons an AndaDB generation.
use super::*;
use anda_engine::memory::ConversationRef;
use futures::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as StoreResult, path::Path,
};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Default)]
struct BlockAfterPut {
    inner: InMemory,
    path: parking_lot::Mutex<String>,
    armed: AtomicBool,
    only_when_readonly: parking_lot::Mutex<Option<Weak<AndaDB>>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail_next: AtomicBool,
    reads: AtomicU64,
}

impl std::fmt::Display for BlockAfterPut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BlockAfterPut")
    }
}

impl BlockAfterPut {
    fn arm(&self, path: &str) {
        *self.path.lock() = path.into();
        self.armed.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ObjectStore for BlockAfterPut {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> StoreResult<PutResult> {
        let phase_matches = self
            .only_when_readonly
            .lock()
            .as_ref()
            .is_none_or(|db| db.upgrade().is_some_and(|db| db.is_read_only()));
        let block = phase_matches
            && path.as_ref().contains(self.path.lock().as_str())
            && self.armed.swap(false, Ordering::SeqCst);
        if block && self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(object_store::Error::Generic {
                store: "close-test",
                source: std::io::Error::other("injected close failure").into(),
            });
        }
        let result = self.inner.put_opts(path, payload, options).await?;
        if block {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> StoreResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }
    async fn get_opts(&self, path: &Path, options: GetOptions) -> StoreResult<GetResult> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_opts(path, options).await
    }
    fn delete_stream(
        &self,
        paths: BoxStream<'static, StoreResult<Path>>,
    ) -> BoxStream<'static, StoreResult<Path>> {
        self.inner.delete_stream(paths)
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, StoreResult<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> StoreResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> StoreResult<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

async fn fixture(name: &str) -> (AppState, Arc<Space>, Arc<BlockAfterPut>) {
    let store = Arc::new(BlockAfterPut::default());
    let app = crate::testkit::app_state_core(name, Arc::new(Models::default()), vec![], "test", 0)
        .fork_with_store(store.clone());
    let space = crate::testkit::create_loaded_space(&app, name).await;
    (app, space, store)
}

async fn record(collection: &Collection, status: ConversationStatus, label: &str) -> u64 {
    collection
        .add_from(&Conversation {
            user: SELF_USER_ID,
            status,
            label: Some(label.into()),
            ..Default::default()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn close_recovers_a_cancelled_conversation_put_and_persists_unknown_terminal_state() {
    let (app, space, store) = fixture("close_conversation").await;
    let id = record(
        &space.recall.conversations_collection,
        ConversationStatus::Working,
        "recall",
    )
    .await;
    space
        .recall
        .conversations_collection
        .update(
            id,
            BTreeMap::from([(
                "failed_reason".into(),
                Fv::Text("prior provider failure".into()),
            )]),
        )
        .await
        .unwrap();
    let completed = record(
        &space.recall.conversations_collection,
        ConversationStatus::Completed,
        "complete",
    )
    .await;
    let queued = record(
        &space.conversations,
        ConversationStatus::Submitted,
        "formation",
    )
    .await;
    space.flush().await.unwrap();
    let original = space.recall.conversations_collection.clone();
    let writer = original.clone();
    store.arm("close_conversation/recall/data/");
    space.tasks.spawn(async move {
        let _ = writer
            .update(
                id,
                BTreeMap::from([("label".into(), Fv::Text("durable-before-cancel".into()))]),
            )
            .await;
    });
    tokio::time::timeout(Duration::from_secs(10), store.entered.notified())
        .await
        .unwrap();
    // A finite test watchdog detects a broken close without hanging the suite.
    tokio::time::timeout(Duration::from_secs(10), space.close())
        .await
        .unwrap()
        .unwrap();
    assert!(
        original.is_poisoned(),
        "the fault must cancel a real mutating future"
    );
    space.close().await.unwrap();

    let reopened = app
        .fork_with_store(store)
        .load_space_with("close_conversation", false, false)
        .await
        .unwrap();
    let restored = reopened
        .recall
        .conversations
        .get_conversation(id)
        .await
        .unwrap();
    assert_eq!(restored.label.as_deref(), Some("durable-before-cancel"));
    assert_eq!(restored.status, ConversationStatus::Cancelled);
    let reason = restored.failed_reason.unwrap();
    assert!(reason.contains("outcome_unknown"));
    assert!(reason.contains("prior provider failure"));
    assert_eq!(
        reopened
            .recall
            .conversations
            .get_conversation(completed)
            .await
            .unwrap()
            .status,
        ConversationStatus::Completed
    );
    assert_eq!(
        reopened
            .memory
            .get_conversation(queued)
            .await
            .unwrap()
            .status,
        ConversationStatus::Submitted
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn close_recovers_a_durable_native_commit_without_replaying_the_formation() {
    let (app, space, store) = fixture("close_native_commit").await;
    let id = space
        .memory
        .add_conversation(ConversationRef::from(&Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Working,
            label: Some("formation".into()),
            ..Default::default()
        }))
        .await
        .unwrap();
    space.flush().await.unwrap();
    let original = space.memory.nexus().store.commit_log();
    let nexus = space.memory.nexus().clone();
    store.arm("close_native_commit/kip_commit_log/data/");
    space.tasks.spawn(async move {
        let _ = execute_request(
            nexus.as_ref(),
            &kip::request(
                r#"MUTATE {
            CREATE CONCEPT ?p {TYPE "Person" NAME "effect committed before close"}
        }"#,
            ),
        )
        .await;
    });
    tokio::time::timeout(Duration::from_secs(10), store.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), space.close())
        .await
        .unwrap()
        .unwrap();
    assert!(original.is_poisoned());
    let reopened = app
        .fork_with_store(store)
        .load_space_with("close_native_commit", false, false)
        .await
        .unwrap();
    let response = reopened
        .execute_kip_readonly(kip::request(
            r#"FIND(?p) WHERE {
        ?p CONCEPT {name: "effect committed before close"}
    }"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        crate::assess::single_read_result(&response)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened.memory.get_conversation(id).await.unwrap().status,
        ConversationStatus::Cancelled
    );
    assert!(
        reopened.restart_formation(SELF_USER_ID, id).await.is_err(),
        "unknown work must not automatically rerun"
    );
    reopened.close().await.unwrap();
}

#[derive(Debug)]
struct BlockedModel;
impl anda_engine::model::CompletionFeaturesDyn for BlockedModel {
    fn model_name(&self) -> String {
        "blocked-close".into()
    }
    fn completion(
        &self,
        _: anda_core::CompletionRequest,
    ) -> anda_core::BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn close_can_retry_after_its_own_checkpoint_put_was_cancelled() {
    let (app, space, store) = fixture("close_retry").await;
    let id = record(
        &space.recall.conversations_collection,
        ConversationStatus::Working,
        "recall",
    )
    .await;
    space.flush().await.unwrap();
    *store.only_when_readonly.lock() = Some(Arc::downgrade(&space.db));
    store.arm("close_retry/recall/meta.cbor");
    let closing = space.clone();
    let task = tokio::spawn(async move { closing.close().await });
    tokio::time::timeout(Duration::from_secs(10), store.entered.notified())
        .await
        .unwrap();
    assert!(
        space.db.is_read_only(),
        "interrupt the storage-close phase after reconciliation"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(space.recall.conversations_collection.is_poisoned());
    tokio::time::timeout(Duration::from_secs(10), space.close())
        .await
        .unwrap()
        .unwrap();
    space.close().await.unwrap();
    let reopened = app
        .fork_with_store(store)
        .load_space_with("close_retry", false, false)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .recall
            .conversations
            .get_conversation(id)
            .await
            .unwrap()
            .status,
        ConversationStatus::Cancelled
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn close_captures_an_active_submitted_formation_before_the_guard_is_dropped() {
    let app = crate::testkit::app_state_core(
        "close_submitted",
        crate::testkit::models_with_completer(BlockedModel),
        vec![],
        "test",
        0,
    )
    .fork_with_store(Arc::new(InMemory::new()));
    let space = crate::testkit::create_loaded_space(&app, "close_submitted").await;
    let output = space
        .ingest(SELF_USER_ID, StringOr::String("remember this".into()))
        .await
        .unwrap();
    let id = output.conversation.unwrap();
    assert_eq!(space.formation.processing_id(), id);
    space.close().await.unwrap();
    let reopened = app
        .fork_with_store(app.object_store.clone())
        .load_space_with("close_submitted", false, false)
        .await
        .unwrap();
    let conversation = reopened.memory.get_conversation(id).await.unwrap();
    assert_eq!(conversation.status, ConversationStatus::Cancelled);
    assert!(
        conversation
            .failed_reason
            .unwrap()
            .contains("outcome_unknown")
    );
    assert!(reopened.restart_formation(SELF_USER_ID, id).await.is_err());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn warm_space_load_does_not_repeat_directory_reads() {
    let (app, space, store) = fixture("warm_discovery").await;
    let before = store.reads.load(Ordering::SeqCst);
    for _ in 0..10 {
        let loaded = app
            .load_space_with("warm_discovery", false, false)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&space, &loaded));
    }
    assert_eq!(store.reads.load(Ordering::SeqCst), before);
    space.close().await.unwrap();
}

#[tokio::test]
async fn failed_eviction_retains_owner_until_close_retry_succeeds() {
    let (app, space, store) = fixture("eviction_failure").await;
    space
        .db
        .set_extension_from("review_close_marker".into(), "retained");
    let entry = app
        .spaces
        .read()
        .await
        .get("eviction_failure")
        .unwrap()
        .clone();
    entry.last_access_ms.store(0, Ordering::Relaxed);
    let owner = Arc::downgrade(&space);
    drop(space);
    store.fail_next.store(true, Ordering::SeqCst);
    store.arm("eviction_failure/");
    assert!(
        !app.try_evict_idle_space("eviction_failure", &entry, unix_ms(), 1)
            .await
    );
    assert!(entry.closing.load(Ordering::Acquire));
    assert!(owner.upgrade().is_some());
    assert!(app.load_space("eviction_failure", false).await.is_err());
    assert!(
        app.try_evict_idle_space("eviction_failure", &entry, unix_ms(), 1)
            .await
    );
    drop(entry);
    assert!(owner.upgrade().is_none());
    let reopened = app
        .load_space_with("eviction_failure", false, false)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .db
            .get_extension_as::<String>("review_close_marker")
            .as_deref(),
        Some("retained")
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn slow_eviction_does_not_hold_the_map_lock_or_allow_a_second_owner() {
    let (app, space, store) = fixture("eviction_slow").await;
    let other = crate::testkit::create_loaded_space(&app, "eviction_other").await;
    space
        .db
        .set_extension_from("review_close_marker".into(), true);
    let entry = app
        .spaces
        .read()
        .await
        .get("eviction_slow")
        .unwrap()
        .clone();
    entry.last_access_ms.store(0, Ordering::Relaxed);
    drop(space);
    store.arm("eviction_slow/");
    let evict_app = app.clone();
    let eviction = tokio::spawn(async move {
        evict_app
            .try_evict_idle_space("eviction_slow", &entry, unix_ms(), 1)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), store.entered.notified())
        .await
        .unwrap();
    let loaded = tokio::time::timeout(
        Duration::from_secs(1),
        app.load_space_with("eviction_other", false, false),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(Arc::ptr_eq(&loaded, &other));
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            app.load_space_with("eviction_slow", false, false)
        )
        .await
        .unwrap()
        .is_err()
    );
    store.release.notify_one();
    assert!(eviction.await.unwrap());
    other.close().await.unwrap();
}
