use super::*;
use futures::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as StoreResult,
    memory::InMemory, path::Path,
};
#[derive(Debug, Default)]
pub(super) struct CheckpointFault {
    inner: InMemory,
    pub fail_checkpoint: AtomicBool,
    pub lose_dispatch_ack: AtomicBool,
    pub fail_verdict_checkpoint: AtomicBool,
    pub fail_enrollment_create: AtomicBool,
    pub fail_missing_job_read: AtomicBool,
    pub dispatch_checkpoint_delay_ms: std::sync::atomic::AtomicU64,
}
impl std::fmt::Display for CheckpointFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CheckpointFault")
    }
}
#[async_trait::async_trait]
impl ObjectStore for CheckpointFault {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> StoreResult<PutResult> {
        let bytes = payload
            .iter()
            .flat_map(|b| b.iter().copied())
            .collect::<Vec<_>>();
        let value = serde_json::from_slice::<Json>(&bytes).unwrap_or(Json::Null);
        if path.as_ref().contains("/learning/jobs/") {
            if value["stage"] == "installing"
                && matches!(options.mode, object_store::PutMode::Create)
                && self.fail_enrollment_create.swap(false, Ordering::SeqCst)
            {
                return Err(object_store::Error::Generic {
                    store: "checkpoint-fault",
                    source: "injected child enrollment create failure".into(),
                });
            }
            if !value["evaluation_ref"].is_null()
                && value["verdict_pending"].is_null()
                && self.fail_verdict_checkpoint.swap(false, Ordering::SeqCst)
            {
                return Err(object_store::Error::Generic {
                    store: "checkpoint-fault",
                    source: "injected post-native-verdict checkpoint failure".into(),
                });
            }
            if value["outcome_cursor"] == 1
                && value["pending"].is_null()
                && self.fail_checkpoint.swap(false, Ordering::SeqCst)
            {
                return Err(object_store::Error::Generic {
                    store: "checkpoint-fault",
                    source: "injected pre-PUT checkpoint failure".into(),
                });
            }
            let dispatching = value["attempts"]
                .as_object()
                .is_some_and(|a| a.values().any(|v| v["state"] == "dispatched"));
            if dispatching {
                let delay = self.dispatch_checkpoint_delay_ms.swap(0, Ordering::SeqCst);
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
            if dispatching && self.lose_dispatch_ack.swap(false, Ordering::SeqCst) {
                self.inner.put_opts(path, payload, options).await?;
                return Err(object_store::Error::Generic {
                    store: "checkpoint-fault",
                    source: "durable PUT with lost ACK".into(),
                });
            }
        }
        self.inner.put_opts(path, payload, options).await
    }
    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> StoreResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }
    async fn get_opts(&self, path: &Path, options: GetOptions) -> StoreResult<GetResult> {
        let result = self.inner.get_opts(path, options).await;
        if path.as_ref().contains("/learning/jobs/")
            && matches!(&result, Err(object_store::Error::NotFound { .. }))
            && self.fail_missing_job_read.swap(false, Ordering::SeqCst)
        {
            return Err(object_store::Error::Generic {
                store: "checkpoint-fault",
                source: "injected uncertain child enrollment lookup".into(),
            });
        }
        result
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
