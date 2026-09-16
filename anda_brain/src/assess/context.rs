//! Model routing and read access for online diagnostics.
use crate::space::Space;
use anda_core::{AgentOutput, BoxError, CompletionRequest};
use anda_kip::{Request, Response};

/// Minimal capabilities the assessment instruments need from their host:
/// diagnostic model completions and read-only KIP probes. `Space` supplies
/// its own model or explicitly configured independent diagnostic judge.
#[async_trait::async_trait]
pub trait AssessContext: Send + Sync {
    /// Diagnostic completion. Hosts without a model can leave the default.
    async fn complete(&self, _req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        Err("assess context does not support LLM completions".into())
    }

    /// Completion used by judges. Defaults to [`Self::complete`]; hosts with
    /// an independent judge model override this (plan M9), so judge scores
    /// stop sharing the evaluated system's blind spots.
    async fn judge_complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        self.complete(req).await
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError>;
}

#[async_trait::async_trait]
impl AssessContext for Space {
    async fn complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        self.diagnostic_complete(req).await
    }

    async fn judge_complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        match self.judge_model() {
            Some(model) => model.completion(req).await,
            None => self.diagnostic_complete(req).await,
        }
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError> {
        // Inherent method (space.rs); takes priority over this trait method.
        self.execute_kip_readonly(request).await
    }
}
