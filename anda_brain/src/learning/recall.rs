//! Model-facing read seam. No context, scores, authorization or standing writes
//! are accepted through this tool; deployment bindings remain trusted host APIs.
use super::{LearningRuntime, ProcedureStatus};
use anda_core::{BoxError, FunctionDefinition, Resource, Tool, ToolOutput};
use anda_engine::context::BaseCtx;
use serde::Deserialize;
use serde_json::json;
use std::{sync::Arc, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureArgs {
    pub skill_ref: String,
}

pub struct ProcedureStatusTool(pub Arc<LearningRuntime>);
impl ProcedureStatusTool {
    pub const NAME: &'static str = "check_procedure_status";
}
impl Tool<BaseCtx> for ProcedureStatusTool {
    type Args = ProcedureArgs;
    type Output = ProcedureStatus;

    fn name(&self) -> String {
        Self::NAME.into()
    }
    fn description(&self) -> String {
        "Check one recalled Skill's current revision, verified adoption, review deadline, dependencies and host-observed application context. Read-only; neither recall nor adoption grants execution permission. Call before recommending a procedure. If unavailable or recommendation_allowed is false, describe it as unproven, unverifiable, expired or a warning, with the returned reason.".into()
    }
    fn definition(&self) -> FunctionDefinition {
        serde_json::from_value(json!({
            "name":Self::NAME,"description":self.description(),"strict":true,
            "parameters":{"type":"object","properties":{
                "skill_ref":{"type":"string","description":"Exact Skill Concept reference returned by memory recall, e.g. C-12. This is the Skill, not its revision."}
            },"required":["skill_ref"],"additionalProperties":false}
        })).expect("static procedure tool schema")
    }
    async fn call(
        &self,
        _ctx: BaseCtx,
        args: ProcedureArgs,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<ProcedureStatus>, BoxError> {
        let status = tokio::time::timeout(
            Duration::from_secs(5),
            self.0.procedure_status(&args.skill_ref),
        )
        .await
        .map_err(|_| "procedure applicability read timed out; recommendation is unverified")??;
        Ok(ToolOutput::new(status))
    }
}
