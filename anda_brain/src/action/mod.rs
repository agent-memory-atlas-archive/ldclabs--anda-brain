//! Trusted four-way decisions and native, fenced external dispatch.
//! No adapter is installed by default. A DecisionRecord is never authority.
use anda_cognitive_nexus::{
    attention::{
        DispatchLookup, DispatchLookupObserver, RuntimePin, RuntimePins, RuntimeScope, WakeRecord,
    },
    governance::AuthContext,
};
use anda_core::{BoxError, Json};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

mod context;
mod dispatch;
mod gate;
mod inbox;
mod journal;
mod native;
mod service;
pub(crate) use inbox::Inspection;
pub use service::ActionRuntime;

const FORMAT: &str = "anda-brain:action-v1";
use crate::PROFILE;

/// Code-only installation. The callbacks and all destinations belong to the
/// host. Never deserialize these from a model tool request.
#[derive(Clone)]
pub struct ActionBindings {
    pub policy_pin: RuntimePin,
    /// Pins the complete business/clarification adapter set, including targets.
    pub binding_pin: RuntimePin,
    pub policy: Arc<dyn ActionPolicy>,
    pub identity: Arc<dyn ActionIdentity>,
    pub business: Option<Arc<dyn ActionExecutor>>,
    pub clarification: Option<ClarificationBinding>,
    pub lookup: Option<LookupBinding>,
    pub limits: ActionLimits,
}

#[derive(Clone)]
pub struct ClarificationBinding {
    pub executor: Arc<dyn ActionExecutor>,
    /// The authenticated recipient; suggestions cannot select another account.
    pub recipient_principal: String,
    pub reply_timeout_ms: u64,
}

#[derive(Clone)]
pub struct LookupBinding {
    pub observer: DispatchLookupObserver,
    pub client: Arc<dyn ActionLookup>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionLimits {
    pub recall: crate::recall_budget::RecallBudget,
    pub callbacks_ms: u64,
    pub lease_ms: u64,
    pub retry_ms: u64,
    pub max_retries: u32,
    pub per_pass: usize,
    pub proposal_tokens: usize,
}
impl Default for ActionLimits {
    fn default() -> Self {
        Self {
            recall: Default::default(),
            callbacks_ms: 5_000,
            lease_ms: 60_000,
            retry_ms: 60_000,
            max_retries: 3,
            per_pass: 2,
            proposal_tokens: 2_048,
        }
    }
}
impl ActionLimits {
    pub fn validate(&self) -> Result<(), BoxError> {
        self.recall.validate()?;
        let l = self;
        if !(1..=30_000).contains(&l.callbacks_ms)
            || !(1_000..=300_000).contains(&l.lease_ms)
            || l.lease_ms <= l.callbacks_ms.saturating_mul(4)
            || !(1..=3_600_000).contains(&l.retry_ms)
            || l.max_retries > 16
            || !(1..=20).contains(&l.per_pass)
            || !(1..=8_192).contains(&l.proposal_tokens)
        {
            return Err("invalid bounded action limits".into());
        }
        Ok(())
    }
}
impl ActionBindings {
    pub fn validate(&self) -> Result<(), BoxError> {
        self.limits.validate()?;
        for pin in [&self.policy_pin, &self.binding_pin] {
            if pin.id.is_empty() || pin.id.len() > 256 || !digest_valid(&pin.digest) {
                return Err("action configuration requires bounded versioned pins".into());
            }
        }
        if let Some(c) = &self.clarification
            && (!principal_valid(&c.recipient_principal)
                || !(1..=604_800_000).contains(&c.reply_timeout_ms))
        {
            return Err(
                "clarification requires an authenticated recipient and bounded deadline".into(),
            );
        }
        if let Some(l) = &self.lookup
            && (l.observer.binding != self.binding_pin
                || !principal_valid(&l.observer.principal_id)
                || !digest_valid(&l.observer.configuration_digest))
        {
            return Err("lookup observer must pin this adapter set".into());
        }
        Ok(())
    }
    pub fn pins(&self) -> RuntimePins {
        RuntimePins {
            policy: RuntimePin {
                id: self.policy_pin.id.clone(),
                digest: anda_cognitive_nexus::content_digest(&self.manifest())
                    .expect("JSON configuration is canonicalizable"),
            },
            evaluator: None,
            binding: Some(self.binding_pin.clone()),
        }
    }
    fn manifest(&self) -> Json {
        serde_json::json!({"format":FORMAT,"policy":self.policy_pin,"binding":self.binding_pin,"limits":self.limits,
            "business":self.business.is_some(),"business_idempotency":self.business.as_ref().map(|e| e.supports_idempotency()),
            "clarification":self.clarification.as_ref().map(|c| serde_json::json!({"recipient":c.recipient_principal,"timeout_ms":c.reply_timeout_ms,"idempotency":c.executor.supports_idempotency()})),
            "lookup":self.lookup.as_ref().map(|l| &l.observer)})
    }
}

/// A fresh authentication result for each operation, never a stored credential.
#[async_trait]
pub trait ActionIdentity: Send + Sync {
    async fn authenticate(&self, scope: &RuntimeScope) -> Result<AuthContext, BoxError>;
}

/// Required read scope is assigned by trusted code, not by a model suggestion.
/// The anchor is an existing Proposition used only as a native read coordinate;
/// it need not be true. Actual prerequisites are separately listed in premises.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_receipt: Option<crate::recall_receipt::RecallReceiptRef>,
    pub anchor: String,
    pub required_refs: Vec<String>,
    pub premises: Vec<String>,
    pub applied_revisions: Vec<String>,
    pub task_family: String,
    pub environment_digest: String,
    pub tool_versions: BTreeMap<String, String>,
    /// Identifies one logical business operation within this Space instance.
    pub deduplication_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateInput {
    pub wake: WakeRecord,
    pub packet: crate::recall_budget::MemoryPacket,
    pub reply: Option<ClarificationResponse>,
    pub proposal_token_limit: usize,
}

/// Only `suggest` may be backed by a model. `context` and `authorize` are
/// trusted host policy. An adapter must bound any model's own request/output.
#[async_trait]
pub trait ActionPolicy: Send + Sync {
    async fn context(&self, wake: &WakeRecord) -> Result<ContextRequest, BoxError>;
    async fn suggest(&self, input: &GateInput) -> Result<Proposal, BoxError>;
    async fn authorize(&self, request: &ActionRequest) -> Result<(), BoxError>;
    /// Silence requires an explicit host rule, independently of a suggestion.
    async fn allow_silence(&self, input: &GateInput, reason: &str) -> Result<bool, BoxError> {
        let _ = (input, reason);
        Ok(false)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum Proposal {
    Act {
        rationale: String,
        used_refs: Vec<String>,
        payload: Json,
    },
    Ask {
        rationale: String,
        used_refs: Vec<String>,
        question: String,
    },
    Defer {
        reason: String,
    },
    Silence {
        rationale: String,
        used_refs: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Business,
    DeliverClarification,
}

/// The target/destination is chosen by the installed adapter. `payload` is
/// content, never target-system authority. The stable key is the native attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub scope: RuntimeScope,
    pub pins: RuntimePins,
    pub gate_wake_ref: String,
    pub kind: ActionKind,
    pub attempt_id: String,
    pub payload: Json,
    pub context: ContextRequest,
}

#[derive(Clone, Debug)]
pub struct DispatchPermit {
    pub wake_ref: String,
    pub attempt_ref: String,
    pub dispatch_ref: String,
    pub fence: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Accepted,
    Finished,
    Unknown,
}

#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// True only when the actual target has a verified idempotency contract.
    fn supports_idempotency(&self) -> bool {
        false
    }
    /// Check current target permissions, environment/tool versions and budget.
    async fn authorize(&self, request: &ActionRequest) -> Result<(), BoxError>;
    /// Recheck target authority and lease deadline at the actual side effect.
    /// ACK/Finished is transport status; it is never an independent Outcome.
    async fn dispatch(
        &self,
        request: &ActionRequest,
        permit: &DispatchPermit,
    ) -> Result<DeliveryStatus, BoxError>;
}

#[async_trait]
pub trait ActionLookup: Send + Sync {
    /// Read the actual target using this exact attempt identity. Authentication
    /// is freshly established by the adapter, not reconstructed from a journal.
    async fn lookup(
        &self,
        request: &ActionRequest,
        dispatch_ref: &str,
    ) -> Result<(AuthContext, DispatchLookup), BoxError>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClarificationResponse {
    pub event_key: String,
    pub answer: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionStatus {
    pub wake_ref: String,
    pub decision: Option<String>,
    pub decision_ref: Option<String>,
    pub attempt_ref: Option<String>,
    pub dispatch_ref: Option<String>,
    pub state: String,
    pub reason: Option<String>,
    pub next_run_ms: Option<u64>,
    pub attempts: u32,
    /// Explicit operator retry windows; attempts counts the current window.
    #[serde(default)]
    pub operator_retries: u32,
}

fn digest_valid(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|v| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn dispatch_reference(scope: &RuntimeScope, attempt_id: &str) -> Result<String, BoxError> {
    Ok(format!(
        "dispatch/v1/{}",
        &anda_cognitive_nexus::content_digest(
            &serde_json::json!({"scope":scope,"attempt_id":attempt_id})
        )?[7..]
    ))
}
fn principal_valid(value: &str) -> bool {
    value.starts_with("kip:principal:")
        && value.len() <= 256
        && value != anda_cognitive_nexus::governance::SYSTEM_PRINCIPAL
        && value != anda_cognitive_nexus::governance::ANONYMOUS_PRINCIPAL
}
