//! Trusted scheduling seams. None of these callbacks is a model tool.
use super::*;
use anda_cognitive_nexus::governance::AuthContext;
use anda_core::BoxError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LearningAutomation {
    pub trials: bool,
    pub reviews: bool,
    pub archive: bool,
    pub safety: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentOrigin {
    pub source_id: String,
    pub source_digest: String,
    pub event_key: String,
    pub trigger_ref: Option<String>,
    pub gate_wake_ref: Option<String>,
}
impl EnrollmentOrigin {
    pub fn validate(&self) -> Result<(), BoxError> {
        if [self.source_id.as_str(), self.event_key.as_str()]
            .iter()
            .any(|v| v.is_empty() || v.len() > 256)
            || !super::is_digest(&self.source_digest)
            || self.trigger_ref.as_ref().is_some_and(|r| {
                !r.strip_prefix("X-")
                    .is_some_and(|v| v.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == v))
            })
            || self.gate_wake_ref.as_ref().is_some_and(|r| {
                !r.strip_prefix("wake/v1/")
                    .is_some_and(|v| super::is_digest(&format!("sha256:{v}")))
            })
        {
            return Err("invalid registered learning trigger".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningEnrollment {
    pub job_id: String,
    pub plan: PairedTrialPlan,
    pub basis_proposition: String,
    pub origin: EnrollmentOrigin,
}

#[async_trait]
pub trait LearningPlanFactory: Send + Sync {
    fn source(&self) -> (String, String);
    /// Return a frozen host-authorized manifest, or None when no fresh cohort
    /// exists. No scheduler invents cases or extends an old observation window.
    async fn next(
        &self,
        instance: &str,
        after: Option<&str>,
        review: Option<&ReviewStatus>,
        cancel: CancellationToken,
    ) -> Result<Option<LearningEnrollment>, BoxError>;
}
#[async_trait]
pub trait LearningObserver: Send + Sync {
    fn control(&self) -> anda_kip::cognitive::ObserverControl;
    /// Reauthenticate the independently controlled instrument on every pass.
    async fn authenticate(&self, cancel: CancellationToken) -> Result<AuthContext, BoxError>;
    async fn observe(
        &self,
        ticket: &DispatchTicket,
        cancel: CancellationToken,
    ) -> Result<Option<LearningObservation>, BoxError>;
}
pub struct LearningObservation {
    pub input: crate::consequence::OutcomeInput,
    /// Protected host replay material, never forwarded to the business model.
    /// Its canonical digest must equal the instrument's journal_digest.
    pub replay: anda_core::Json,
}

/// Operator-reviewed MIB material, retained with the trusted deployment.
/// This validates provenance/pins and explicit approval; it does not turn a
/// synthetic report or an operator assertion into empirical improvement.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningCalibration {
    pub format: String,
    pub reviewed_by: String,
    pub approved_for_automatic_trials: bool,
    pub contract_digest: String,
    pub environment_digest: String,
    pub training_manifest: anda_core::Json,
    pub validation_manifest: anda_core::Json,
    pub report: anda_core::Json,
}
pub fn calibration_contract(config: &LearningConfig) -> Result<String, BoxError> {
    Ok(anda_cognitive_nexus::content_digest(&serde_json::json!({
        "task_family":config.task_family,"model":config.model_digest,"tools":config.tool_versions,
        "budget":config.budget,"observer":config.observer,"rule":config.rule_digest,
        "alpha":config.alpha,"minimum_effect":config.minimum_effect,"maximum_failure_rate":config.maximum_failure_rate,
        "minimum_pairs":config.minimum_pairs,"maximum_pairs":config.maximum_pairs
    }))?)
}
#[derive(Clone)]
pub struct LearningBindings {
    pub registration: LearningConfig,
    pub automation: LearningAutomation,
    pub storage: LearningStoragePolicy,
    pub executor: Arc<dyn LearningExecutor>,
    pub observer: Arc<dyn LearningObserver>,
    pub plans: Arc<dyn LearningPlanFactory>,
    /// Actual operator-reviewed material, not a fixture digest or model claim.
    pub calibration: anda_core::Json,
    pub callback_timeout_ms: u64,
}
impl LearningBindings {
    pub fn validate(&self) -> Result<(), BoxError> {
        self.registration.validate()?;
        self.storage.validate()?;
        let control = self.observer.control();
        let identity = self.executor.identity();
        let (source, digest) = self.plans.source();
        let material: LearningCalibration = serde_json::from_value(self.calibration.clone())?;
        if material.format != "anda-brain:learning-calibration-v1"
            || !crate::runtime_api::principal_valid(&material.reviewed_by)
            || material.reviewed_by == identity.principal_id
            || material.reviewed_by == control.principal_id
            || (!material.approved_for_automatic_trials
                && (self.automation.trials || self.automation.reviews))
            || material.contract_digest != calibration_contract(&self.registration)?
            || material.environment_digest != identity.environment_digest
            || !material.training_manifest.is_object()
            || !material.validation_manifest.is_object()
            || !material.report.is_object()
            || material.training_manifest == material.validation_manifest
        {
            return Err("automatic learning requires explicitly reviewed calibration and distinct frozen train/validation material for this contract".into());
        }
        if anda_cognitive_nexus::content_digest(&self.calibration)?
            != self.registration.calibration_digest
            || !self.calibration.is_object()
            || serde_json::to_vec(&self.calibration)?.len() > 262_144
            || serde_json::to_value(&control)? != serde_json::to_value(&self.registration.observer)?
            || identity.principal_id != self.registration.executor_principal
            || identity.control_domain != self.registration.executor_control_domain
            || identity.model_digest != self.registration.model_digest
            || identity.tool_versions != self.registration.tool_versions
            || identity.budget_digest != self.registration.budget.digest()?
            || source.is_empty()
            || source.len() > 256
            || !super::is_digest(&digest)
            || !(1..=30_000).contains(&self.callback_timeout_ms)
            || self.registration.budget.elapsed_ms > self.callback_timeout_ms
        {
            return Err("learning bindings require matching executor, independent observer, calibration, source and bounded cancellation".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LearningPass {
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub enrolled: usize,
    pub driven: usize,
    pub observed: usize,
    pub settled: usize,
    pub archived: usize,
    pub reviews_checked: usize,
    pub reviews_enrolled: usize,
    pub safety_resolved: usize,
    pub blocked: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LearningRuntimeStatus {
    /// Product readiness is a snapshot, never permission to skip native per-job checks.
    pub product_readiness: serde_json::Value,
    pub compiled: bool,
    pub registered: bool,
    pub registration_enabled: bool,
    pub bindings_ready: bool,
    pub automatic_allowed: bool,
    pub running: bool,
    pub automation: LearningAutomation,
    pub blocked_reasons: Vec<String>,
    pub capacity: Option<LearningCapacity>,
    pub last_pass: Option<LearningPass>,
}
