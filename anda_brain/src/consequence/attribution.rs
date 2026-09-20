//! Versioned causal claims are accepted only from the configured independent
//! native observer. Retrieval diagnostics are not a contribution measurement.
use super::*;
use crate::recall_receipt::{MemoryPin, RecallReceiptRef};
use anda_cognitive_nexus::content_digest;
use serde_json::json;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttributionMethod {
    SingleContributionV1,
    PairedRevisionV1,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UtilityParameters {
    pub step_cap: f64,
    pub minimum_independent_samples: usize,
    pub gain: f64,
    pub minimum_confidence: f64,
    /// An admission assumption, retained as such in every first calibration.
    pub initial_utility: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UtilityCalibration {
    pub reviewed_by: String,
    pub contract_digest: String,
    pub approved: bool,
    pub material: Json,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UtilityConfig {
    pub version: String,
    pub method: AttributionMethod,
    pub observer: anda_kip::cognitive::ObserverControl,
    pub task_family: String,
    pub metric: String,
    pub window: String,
    pub environment_digest: String,
    pub tool_versions: std::collections::BTreeMap<String, String>,
    pub parameters: Option<UtilityParameters>,
    pub calibration: Option<UtilityCalibration>,
    pub automatic: bool,
    pub apply: bool,
    pub rank: bool,
}
impl UtilityConfig {
    pub fn contract_digest(&self) -> Result<String, BoxError> {
        Ok(content_digest(
            &json!({"version":self.version,"method":self.method,"observer":self.observer,
            "task_family":self.task_family,"metric":self.metric,"window":self.window,"environment":self.environment_digest,
            "tools":self.tool_versions,"parameters":self.parameters,
            "selection":"fixed-first-n-unique-unconsumed-roots; one-complete-paired-trial; current-terminal-uncontested-evidence; exact-memory-content-v1",
            "uncertainty":"weighted-interval-union-bound; paired-two-sided-hoeffding-v1",
            "mapping":"clamp(old+clip(gain*mean_effect,-step_cap,step_cap),0,1)-v1"}),
        )?)
    }
    pub fn validate(&self) -> Result<(), BoxError> {
        if self.method == AttributionMethod::PairedRevisionV1
            && (!cfg!(feature = "learning")
                || self.task_family != "tool_workflow.precondition.v1"
                || self.metric != "success")
        {
            return Err(
                "paired utility requires learning and the registered workflow contract".into(),
            );
        }
        if self.version.is_empty()
            || self.version.len() > 128
            || !crate::runtime_api::principal_valid(&self.observer.principal_id)
            || !crate::runtime_api::digest_valid(&self.observer.configuration_digest)
            || !crate::runtime_api::digest_valid(&self.environment_digest)
            || self.observer.control_domain.is_empty()
            || self.observer.control_domain.len() > 256
            || [&self.task_family, &self.metric, &self.window]
                .iter()
                .any(|s| s.is_empty() || s.len() > 256)
            || self.tool_versions.is_empty()
            || self.tool_versions.len() > 16
            || self
                .tool_versions
                .iter()
                .any(|(k, v)| k.is_empty() || k.len() > 128 || v.is_empty() || v.len() > 256)
        {
            return Err("invalid utility method/scope/independent observer".into());
        }
        if let Some(p) = &self.parameters
            && (!p.step_cap.is_finite()
                || !(0.0..=1.0).contains(&p.step_cap)
                || p.step_cap == 0.0
                || !p.gain.is_finite()
                || !(0.0..=1.0).contains(&p.gain)
                || p.gain == 0.0
                || !(1..=64).contains(&p.minimum_independent_samples)
                || !p.minimum_confidence.is_finite()
                || !(0.0..1.0).contains(&p.minimum_confidence)
                || p.minimum_confidence == 0.0
                || p.initial_utility
                    .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v)))
        {
            return Err(
                "utility needs explicit bounded mapping, sample and uncertainty parameters".into(),
            );
        }
        if let Some(c) = &self.calibration
            && (!crate::runtime_api::principal_valid(&c.reviewed_by)
                || c.reviewed_by == self.observer.principal_id
                || c.contract_digest != self.contract_digest()?
                || !c.material.is_object()
                || serde_json::to_vec(&c.material)?.len() > 16_384)
        {
            return Err("utility calibration does not approve this exact contract".into());
        }
        if self.apply && !self.calibrated() {
            return Err(
                "utility apply requires parameters and explicit calibrated-method approval".into(),
            );
        }
        Ok(())
    }
    pub fn calibrated(&self) -> bool {
        self.parameters.is_some() && self.calibration.as_ref().is_some_and(|c| c.approved)
    }
}

/// Referenced native Evidence must have been written by the independent
/// observer, and contain this exact bounded witness format. The sampling unit
/// belongs to the registered instrument contract, never to an agent's prose.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributionWitness {
    pub format: String,
    pub contract_digest: String,
    pub sampling_unit: String,
    pub attempt_ref: String,
    pub decision_ref: String,
    pub recall_receipt: RecallReceiptRef,
    pub target: MemoryPin,
    pub effect: f64,
    pub lower_bound: f64,
    pub upper_bound: f64,
    pub confidence: f64,
    pub isolated_contribution: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UtilityAttribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness_ref: Option<String>,
    /// A signed independent observation may carry its witness directly in the native
    /// Outcome Evidence payload; business observers need not issue KIP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness: Option<ContributionWitness>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttributionSample {
    pub outcome_ref: String,
    pub attempt_ref: String,
    pub decision_ref: String,
    pub target: MemoryPin,
    pub used_refs: Vec<String>,
    pub applied_revisions: Vec<String>,
    pub recall_receipt: Option<RecallReceiptRef>,
    pub evidence_refs: Vec<String>,
    pub evidence_digests: std::collections::BTreeMap<String, String>,
    pub independent_unit: String,
    pub effect: Option<f64>,
    pub lower_bound: Option<f64>,
    pub upper_bound: Option<f64>,
    pub confidence: Option<f64>,
    pub independent_samples: usize,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalibrationReceipt {
    pub method: AttributionMethod,
    pub parameters: Option<UtilityParameters>,
    pub configuration_digest: String,
    pub comparable_scope: Json,
    pub format: String,
    pub calibration_key: String,
    pub contract_digest: String,
    pub scope: RuntimeScope,
    pub target: String,
    pub target_pin: Option<MemoryPin>,
    pub selected_outcomes: Vec<String>,
    pub excluded: std::collections::BTreeMap<String, String>,
    pub evidence_refs: Vec<String>,
    pub uncertainty: Json,
    pub independent_samples: usize,
    pub old_value: Option<f64>,
    pub new_value: Option<f64>,
    pub initial_assumption: Option<f64>,
    pub delta: Option<f64>,
    pub status: String,
    pub reason: Option<String>,
    pub previous_receipt: Option<String>,
}
