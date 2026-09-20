//! Authenticated observation receipts. Receipt acceptance, native persistence,
//! learning eligibility and positive consequences are different statements.
use anda_cognitive_nexus::attention::RuntimeScope;
use anda_core::{BoxError, Json};
use serde::{Deserialize, Serialize};

mod attribution;
#[cfg(feature = "learning")]
mod learning;
mod native;
pub use attribution::*;
mod service;
pub mod trust;
pub mod utility;
pub use service::ConsequenceRuntime;

pub const FORMAT: &str = "anda-brain:observation-receipt-v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverContract {
    pub principal_id: String,
    pub configuration_digest: String,
    pub control_domain: String,
    pub task_family: String,
    pub metric: String,
    pub window: String,
    /// Observation/arrival after this attempt-relative window is audit only.
    pub maximum_delay_ms: u64,
}
impl ObserverContract {
    pub fn validate(&self) -> Result<(), BoxError> {
        if !crate::runtime_api::principal_valid(&self.principal_id)
            || !crate::runtime_api::digest_valid(&self.configuration_digest)
            || [
                &self.control_domain,
                &self.task_family,
                &self.metric,
                &self.window,
            ]
            .iter()
            .any(|s| s.is_empty() || s.len() > 256)
            || !(1..=31_536_000_000).contains(&self.maximum_delay_ms)
        {
            return Err("invalid bounded observer contract".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Success,
    Partial,
    Failure,
    Aborted,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    Measurement {
        terminal: bool,
        outcome_status: OutcomeStatus,
        magnitude: Option<f64>,
        payload: Json,
    },
    /// The registered paired controller classifies these exact measurements.
    /// The wire variant remains recognizable in lean builds, which refuse it.
    Learning { measurements: Json },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utility: Option<UtilityAttribution>,
    pub space_instance: String,
    pub attempt_ref: String,
    pub observer_configuration_digest: String,
    pub event_key: String,
    pub observed_at: String,
    pub metric: String,
    pub window: String,
    pub observation: Observation,
    #[serde(default)]
    pub correction_of: Option<String>,
    #[serde(default)]
    pub safety_signal: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationReceipt {
    pub format: String,
    pub receipt_id: String,
    pub scope: RuntimeScope,
    pub event_key: String,
    pub body_digest: String,
    pub observer: String,
    pub received_at_ms: u64,
    pub observed_at: String,
    pub status: String,
    pub native_committed: bool,
    pub learning_eligible: bool,
    pub outcome_status: Option<OutcomeStatus>,
    pub outcome_ref: Option<String>,
    pub observation_ref: Option<String>,
    pub reason: Option<String>,
    pub safety_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_evaluation_ref: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub enum ObservationLane {
    Action,
    #[cfg(feature = "learning")]
    Learning,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptPage {
    pub items: Vec<ObservationReceipt>,
    pub next_after: u64,
    pub complete: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    receipt: ObservationReceipt,
    input: OutcomeInput,
    decision_ref: String,
    dispatch_ref: Option<String>,
    command: Option<String>,
    native_key: String,
    contract_digest: String,
}
