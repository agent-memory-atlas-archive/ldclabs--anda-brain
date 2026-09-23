//! Per-host model-call permits, independent of HTTP/MCP request admission.
use anda_core::{AgentOutput, BoxError, BoxPinFut, CompletionRequest, Json};
use anda_engine::model::{CompletionFeaturesDyn, Model, Models};
use std::{collections::BTreeSet, sync::Arc};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

struct LimitedCompletion {
    inner: Arc<dyn CompletionFeaturesDyn>,
    slots: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl CompletionFeaturesDyn for LimitedCompletion {
    fn model_name(&self) -> String {
        self.inner.model_name()
    }

    fn completion(&self, request: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        let inner = self.inner.clone();
        let slots = self.slots.clone();
        let cancel = self.cancel.clone();
        Box::pin(async move {
            let _permit = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err("space model calls are closed".into()),
                permit = slots.acquire_owned() => permit.map_err(|_| "model concurrency budget is closed")?,
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("space model call cancelled".into()),
                output = inner.completion(request) => output,
            }
        })
    }

    fn prune_unanswered_tool_calls(&self, history: &mut Vec<Json>, start: usize) {
        self.inner.prune_unanswered_tool_calls(history, start);
    }
    fn prune_tool_interactions(&self, history: &mut Vec<Json>) {
        self.inner.prune_tool_interactions(history);
    }
}

pub(crate) fn limit(mut model: Model, slots: &Arc<Semaphore>, cancel: &CancellationToken) -> Model {
    model.completer = Arc::new(LimitedCompletion {
        inner: model.completer,
        slots: slots.clone(),
        cancel: cancel.clone(),
    });
    model
}

/// Preserve registry routing for every model name/declared label and the three
/// engine agent labels. Brain does not accept arbitrary model routes in inputs.
pub(crate) fn registry(
    source: &Models,
    slots: &Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Models {
    let result = Models::from_clone(source);
    let mut labels: BTreeSet<String> = source.model_names();
    for name in source.model_names() {
        if let Some(model) = source.get(&name) {
            labels.extend(model.labels);
        }
    }
    labels.extend([
        crate::agents::FormationAgent::NAME.into(),
        crate::agents::RecallAgent::NAME.into(),
        crate::agents::MaintenanceAgent::NAME.into(),
        "primary".into(),
    ]);
    // Resolve all routes against the original registry, never an already
    // wrapped entry (nested acquisition would deadlock at concurrency one).
    for label in labels {
        if let Some(model) = source.get(&label) {
            result.set(label, limit(model, slots, cancel));
        }
    }
    if let Some(mut model) = source.get_model() {
        let name = model.model_name();
        model.labels.clear();
        result.set_model(limit(model, slots, cancel));
        // A named route can use different credentials/provider settings from
        // the default even when both expose the same model name.
        if let Some(named) = source.get(&name) {
            result.set(name, limit(named, slots, cancel));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo(&'static str);
    impl CompletionFeaturesDyn for Echo {
        fn model_name(&self) -> String {
            "shared-name".into()
        }
        fn completion(&self, _: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let content = self.0.to_string();
            Box::pin(async move {
                Ok(AgentOutput {
                    content,
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn registry_preserves_distinct_default_and_named_routes_and_cancels_waiters() {
        let source = Models::default();
        source.set_model(Model::with_completer(Arc::new(Echo("default"))));
        source.set(
            crate::agents::RecallAgent::NAME.into(),
            Model::with_completer(Arc::new(Echo("recall"))),
        );
        let slots = Arc::new(Semaphore::new(1));
        let cancel = CancellationToken::new();
        let wrapped = registry(&source, &slots, &cancel);
        for (model, expected) in [
            (wrapped.get_model().unwrap(), "default"),
            (wrapped.get("shared-name").unwrap(), "recall"),
            (
                wrapped.get(crate::agents::RecallAgent::NAME).unwrap(),
                "recall",
            ),
        ] {
            assert_eq!(
                model
                    .completion(CompletionRequest::default())
                    .await
                    .unwrap()
                    .content,
                expected
            );
        }
        let held = slots.acquire().await.unwrap();
        let model = wrapped.get_model().unwrap();
        let waiting =
            tokio::spawn(async move { model.completion(CompletionRequest::default()).await });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(waiting.await.unwrap().is_err());
        drop(held);
        assert_eq!(slots.available_permits(), 1);
    }
}
