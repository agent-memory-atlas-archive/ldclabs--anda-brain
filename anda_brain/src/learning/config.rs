use super::{AttemptBudget, PairedTrialPlan, invalid, is_digest, paired_rule_artifact};
use anda_cognitive_nexus::content_digest;
use anda_kip::{KipError, cognitive::ObserverControl};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Public runtime identity; credentials belong to the authenticated host, never
/// to a persisted plan, model prompt or outcome payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorIdentity {
    pub principal_id: String,
    pub control_domain: String,
    pub model_digest: String,
    pub environment_digest: String,
    pub base_memory_digest: String,
    pub tool_versions: BTreeMap<String, String>,
    pub budget_digest: String,
}

/// An explicit host registration for the first supported task family. There
/// are no production thresholds or observer defaults. Registration is trusted
/// configuration, not evidence that the calibration itself was sound.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningConfig {
    pub registration_id: String,
    pub task_family: String,
    pub controller_control_domain: String,
    pub executor_principal: String,
    pub executor_control_domain: String,
    pub observer: ObserverControl,
    pub model_digest: String,
    pub tool_versions: BTreeMap<String, String>,
    pub budget: AttemptBudget,
    pub alpha: f64,
    pub minimum_effect: f64,
    pub maximum_failure_rate: f64,
    pub minimum_pairs: usize,
    pub maximum_pairs: usize,
    /// Hot orchestration capacity. Archived identities/evidence are retained
    /// under the separate LearningStoragePolicy and do not consume this quota.
    pub maximum_jobs: usize,
    /// Real-time bound on executor status lookup, independent of business time.
    pub reconcile_timeout_ms: u64,
    /// Pin of the operator-reviewed calibration material. An arbitrary model
    /// or user-supplied digest cannot activate this host-only registration.
    pub calibration_digest: String,
    pub rule_digest: String,
}

impl LearningConfig {
    pub fn validate(&self) -> Result<(), KipError> {
        let bounded = |s: &str| !s.trim().is_empty() && s.len() <= 256;
        if !bounded(&self.registration_id)
            || !bounded(&self.controller_control_domain)
            || !bounded(&self.executor_control_domain)
            || !bounded(&self.observer.control_domain)
            || self.task_family != super::workflow_contract()["task_family"]
            || !self.executor_principal.starts_with("kip:principal:")
            || self.executor_principal == self.observer.principal_id
            || self.observer.principal_id == "kip:principal:system"
            || self.observer.control_domain == self.controller_control_domain
            || self.observer.control_domain == self.executor_control_domain
            || !is_digest(&self.model_digest)
            || !is_digest(&self.calibration_digest)
            || self.rule_digest != content_digest(&paired_rule_artifact())?
            || self.minimum_pairs < 2
            || self.maximum_pairs < self.minimum_pairs
            || self.maximum_pairs > 512
            || !(1..=32).contains(&self.maximum_jobs)
            || !(1..=30_000).contains(&self.reconcile_timeout_ms)
        {
            return Err(invalid("invalid or unsupported learning registration"));
        }
        self.budget.digest()?;
        if !self.alpha.is_finite()
            || !(0.0..1.0).contains(&self.alpha)
            || self.alpha == 0.0
            || !self.minimum_effect.is_finite()
            || !(0.0..=1.0).contains(&self.minimum_effect)
            || self.minimum_effect == 0.0
            || !self.maximum_failure_rate.is_finite()
            || !(0.0..=1.0).contains(&self.maximum_failure_rate)
            || self.tool_versions.is_empty()
            || self.tool_versions.len() > 64
            || self
                .tool_versions
                .iter()
                .any(|(k, v)| !bounded(k) || !bounded(v))
            || !is_digest(&self.observer.configuration_digest)
        {
            return Err(invalid(
                "learning needs explicit calibrated bounds and identity",
            ));
        }
        Ok(())
    }

    pub fn validate_plan(&self, plan: &PairedTrialPlan) -> Result<(), KipError> {
        self.validate()?;
        plan.validate()?;
        if plan.task_family != self.task_family
            || plan.model_digest != self.model_digest
            || plan.tool_versions != self.tool_versions
            || plan.execution.budget != self.budget
            || plan.alpha != self.alpha
            || plan.minimum_effect != self.minimum_effect
            || plan.execution.maximum_failure_rate != self.maximum_failure_rate
            || !(self.minimum_pairs..=self.maximum_pairs).contains(&plan.pairs.len())
            || plan.execution.observer_configuration_digest()? != self.observer.configuration_digest
        {
            return Err(invalid(
                "trial changed its registered family, bounds or observer semantics",
            ));
        }
        Ok(())
    }

    pub fn validate_executor(
        &self,
        plan: &PairedTrialPlan,
        identity: &ExecutorIdentity,
    ) -> Result<(), KipError> {
        self.validate_plan(plan)?;
        let expected = ExecutorIdentity {
            principal_id: self.executor_principal.clone(),
            control_domain: self.executor_control_domain.clone(),
            model_digest: plan.model_digest.clone(),
            environment_digest: plan.environment_digest.clone(),
            base_memory_digest: plan.base_memory_digest.clone(),
            tool_versions: plan.tool_versions.clone(),
            budget_digest: plan.budget_digest.clone(),
        };
        if identity != &expected {
            return Err(invalid("executor no longer matches the frozen runtime"));
        }
        Ok(())
    }
}
