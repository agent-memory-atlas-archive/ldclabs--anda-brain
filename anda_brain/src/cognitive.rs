//! The model's bounded host operations for the Cognitive Memory Profile.
//!
//! No identity, evaluator, observer policy or external-tool capability comes
//! from these arguments. Those remain trusted host configuration.
use anda_cognitive_nexus::nexus::DEFAULT_SPACE;
use anda_core::{BoxError, FunctionDefinition, Json, Resource, Tool, ToolOutput};
use anda_engine::{context::BaseCtx, memory::MemoryManagement, unix_ms};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, LazyLock};

pub(crate) const CAPABILITIES: &str = "Anda Brain uses KIP 2.0 / cognitive-memory@2.0.0. \
This connection exposes the existing Brain API and raw KIP, not the optional five-intent \
Memory Interface; no memory_* bundle or full CognitiveMemory conformance is claimed. \
Procedural candidates remain unproven until independently qualified; trial/evaluation scheduling requires explicit host configuration. \
External dispatch requires installed host bindings and the four-way action gate; memory_runtime status reports this connection's configuration. Model plans cannot write learning records, \
Skill learning pointers, computed lineage, LeaseState or WatchState. Nexus computes dependency validity; a stored review never overrides it. \
Text and mixed-selector Watches require a host-installed semantic evaluator; read memory_runtime status.semantic_attention for this connection's configuration and recovery state. Without that binding, semantic evaluation is unavailable. \
Read constraints, task scope, uncertainty and invalid dependencies must remain visible.";

/// The `strength_policy` pin this deployment writes on every `MnemonicState`
/// base: the standard `kip:strength-half-life-30d` artifact bundled with
/// Nexus, named by its policy id and content digest (Profile §6.1, Spec
/// §59.1). The engine computes `effective_strength` only for a pin it can
/// resolve, and reads `null` for any other.
pub(crate) static STRENGTH_POLICY: LazyLock<Json> = LazyLock::new(|| {
    let artifact: Json =
        serde_json::from_str(anda_cognitive_nexus::profiles::STRENGTH_HALF_LIFE_30D)
            .expect("the bundled strength policy parses");
    json!({
        "artifact_ref": artifact["policy_id"],
        "content_digest": artifact["integrity"]["content_digest"],
    })
});

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeArgs {
    pub operation: String,
    pub target_ref: Option<String>,
    pub expected_version: Option<u64>,
    pub content: Option<Json>,
}

#[derive(Clone)]
pub(crate) struct MemoryRuntimeTool {
    memory: Arc<MemoryManagement>,
    attention: Arc<crate::attention::AttentionRuntime>,
    product_control: Option<Arc<crate::product::control::Control>>,
}

impl MemoryRuntimeTool {
    pub const NAME: &'static str = "memory_runtime";

    pub fn new(
        memory: Arc<MemoryManagement>,
        attention: Arc<crate::attention::AttentionRuntime>,
    ) -> Self {
        Self {
            memory,
            attention,
            product_control: None,
        }
    }

    pub(crate) fn with_product_control(
        mut self,
        control: Arc<crate::product::control::Control>,
    ) -> Self {
        self.product_control = Some(control);
        self
    }

    pub async fn execute(&self, args: RuntimeArgs, maintenance: bool) -> Result<Json, BoxError> {
        if args.operation == "syntax" {
            return Ok(json!({"syntax": anda_kip::KIP_SYNTAX}));
        }
        if args.operation == "status" {
            return tokio::time::timeout(
                crate::agents::READONLY_KIP_TIMEOUT,
                self.attention.runtime_status(),
            )
            .await?;
        }
        if args.operation == "content_digest" {
            let content = args.content.ok_or("content is required")?;
            if serde_json::to_vec(&content)?.len() > 65_536 {
                return Err("content exceeds 64 KiB".into());
            }
            return Ok(json!({"content_digest": anda_cognitive_nexus::content_digest(&content)?}));
        }
        if !maintenance {
            return Err("only Maintenance may manage Watch arming and task leases".into());
        }
        let target = args.target_ref.ok_or("target_ref is required")?;
        let expected = args
            .expected_version
            .filter(|v| *v > 0)
            .ok_or("expected_version must be positive")?;
        let nexus = self.memory.nexus();
        let session = nexus.system_session();
        let operation = async {
            match args.operation.as_str() {
                "arm_watch" => self
                    .attention
                    .arm_watch(target.clone(), expected)
                    .await
                    .map_err(|e| match e.downcast::<anda_kip::KipError>() {
                        Ok(e) => *e,
                        Err(e) => anda_kip::KipError::internal_error(e.to_string()),
                    }),
                "lease_task" => {
                    session
                        .lease_task(
                            DEFAULT_SPACE,
                            &target,
                            expected,
                            &crate::kip::timestamp(unix_ms() + 300_000),
                        )
                        .await
                }
                _ => Err(anda_kip::KipError::unsupported_capability(
                    "unknown memory runtime operation",
                )),
            }
        };
        Ok(
            tokio::time::timeout(std::time::Duration::from_secs(30), operation)
                .await
                .map_err(|_| {
                    anda_kip::KipError::outcome_unknown(
                        "runtime operation timed out; re-read current state before retrying",
                    )
                })??,
        )
    }
}

static DEFINITION: LazyLock<FunctionDefinition> = LazyLock::new(|| {
    serde_json::from_value(json!({
    "name": MemoryRuntimeTool::NAME,
    "description": "Read full KIP syntax or runtime configuration/status, compute a kip-jcs-safe-v1 SHA-256 content digest, or (Maintenance only) arm a Watch / acquire or renew a five-minute SleepTask lease. Create Watch as disarmed and SleepTask as pending first. Mutations require the exact target and its current _system.version. Re-read after every mutation; never infer the new version. Arming starts a new observation generation, so review gaps before re-arming. External work requires separately installed host callbacks and the fenced action gate. This tool cannot install or invoke an executor or grant authority.",
    "parameters": {"type":"object", "additionalProperties":false,
        "properties": {
            "operation":{"type":"string","enum":["syntax","status","content_digest","arm_watch","lease_task"]},
            "target_ref":{"type":["string","null"]},
            "expected_version":{"type":["integer","null"]},
            "content":{"description":"Canonical JSON to digest; for a SkillRevision, all attributes except behavior_digest. Null for other operations."}
        },
        "required":["operation","target_ref","expected_version","content"]
    }
})).unwrap()
});

impl Tool<BaseCtx> for MemoryRuntimeTool {
    type Args = RuntimeArgs;
    type Output = Json;
    fn name(&self) -> String {
        Self::NAME.into()
    }
    fn description(&self) -> String {
        DEFINITION.description.clone()
    }
    fn definition(&self) -> FunctionDefinition {
        DEFINITION.clone()
    }
    async fn call(
        &self,
        ctx: BaseCtx,
        args: RuntimeArgs,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<Json>, BoxError> {
        let _guard = if matches!(args.operation.as_str(), "arm_watch" | "lease_task") {
            if let Some(control) = &self.product_control {
                let guard = control.gate.lock().await;
                control.check(&ctx)?;
                Some(guard)
            } else {
                None
            }
        } else {
            None
        };
        let result = self
            .execute(args, ctx.agent == crate::agents::MaintenanceAgent::NAME)
            .await?;
        Ok(ToolOutput::new(result))
    }
}
