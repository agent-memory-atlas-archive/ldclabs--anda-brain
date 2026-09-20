use super::*;
use anda_cognitive_nexus::attention::WakeContinuation;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreparedGate {
    pub command: String,
    pub parameters: anda_kip::Map<String, Json>,
    pub continuations: Vec<WakeContinuation>,
    pub expected: u64,
    pub fence: u64,
    pub decision: String,
    pub capture: context::Capture,
    pub request: Option<ActionRequest>,
    pub recipient: Option<String>,
    pub reply_deadline_ms: Option<u64>,
    pub clarification_payload: Option<Json>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Job {
    pub format: String,
    pub scope: RuntimeScope,
    pub pins: RuntimePins,
    pub wake_ref: String,
    pub rounds: u32,
    pub failures: u32,
    pub status: ActionStatus,
    pub prepared: Option<PreparedGate>,
    pub outputs: Vec<String>,
    pub delivery: Option<DeliveryStatus>,
}
impl Job {
    pub fn new(wake: &WakeRecord, rounds: u32) -> Self {
        Self {
            format: FORMAT.into(),
            scope: wake.scope.clone(),
            pins: wake.pins.clone(),
            wake_ref: wake.wake_ref.clone(),
            rounds,
            failures: 0,
            status: ActionStatus {
                wake_ref: wake.wake_ref.clone(),
                state: "pending".into(),
                ..Default::default()
            },
            prepared: None,
            outputs: vec![],
            delivery: None,
        }
    }
    pub fn validate(&self, scope: &RuntimeScope, reference: &str) -> Result<(), BoxError> {
        if self.format != FORMAT
            || &self.scope != scope
            || self.wake_ref != reference
            || self.rounds > 17
            || self.outputs.len() > 128
        {
            return Err("action journal identity/format mismatch".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub scope: RuntimeScope,
    pub gate_wake_ref: String,
    pub principal: String,
    pub received_ms: u64,
    pub response: ClarificationResponse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Dedup {
    pub scope: RuntimeScope,
    pub pins: RuntimePins,
    pub owner: String,
    pub request_digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Switch {
    pub scope: RuntimeScope,
    pub enabled: bool,
}

pub(super) fn key(scope: &RuntimeScope, kind: &str, id: &str) -> Result<String, BoxError> {
    Ok(format!(
        "actions/{kind}/{}",
        &anda_cognitive_nexus::content_digest(&serde_json::json!({"scope":scope,"id":id}))?[7..]
    ))
}
