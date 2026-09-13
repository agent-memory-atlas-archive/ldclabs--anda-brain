use anda_cognitive_nexus::content_digest;
use anda_kip::{Json, KipError, cognitive::ArtifactPin};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use super::{ExecutionContract, invalid, is_digest, is_revision};

pub(super) const MAX_PAIRS: usize = 4096;

/// One independently sampled task, executed once per arm from the same state.
/// Repeated observations or tool retries belong to the original attempt.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PairCase {
    pub task_digest: String,
    pub initial_state_digest: String,
    pub seed: String,
}

/// Predeclared task manifest and parameters for `anda-brain:paired-bounded-v2`.
///
/// Persist this *before baseline execution* with `Session::put_artifact` and
/// cite its pin in both arms' AttemptRecord.selection_policy. The exact same
/// content is the TrialRecord.parameters artifact. The host owns enrollment;
/// a candidate must not select cases using either arm's observed outcomes.
///
/// v2 evaluates one fixed cohort with binary bounded-success outcomes and a one-sided
/// Hoeffding bound on paired differences in [-1, 1]. It does not support
/// repeated peeking, adaptive stopping, filtering monitoring windows out of
/// one trial, or cross-family aggregation. A new experiment/review needs a new manifest/trial.
/// The control applies no Skill revisions, and treatment applies only this
/// candidate. Multi-Skill incumbent bundles are not supported by v2; both
/// arms must still retain the same factual memory and stable task policy.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PairedTrialPlan {
    pub task_family: String,
    pub candidate_revision: String,
    pub environment_digest: String,
    pub base_memory_digest: String,
    pub model_digest: String,
    pub budget_digest: String,
    pub tool_versions: BTreeMap<String, String>,
    pub observation_window: String,
    pub pairs: BTreeMap<String, PairCase>,
    pub alpha: f64,
    pub minimum_effect: f64,
    pub execution: ExecutionContract,
}

impl PairedTrialPlan {
    pub fn validate(&self) -> Result<(), KipError> {
        self.execution.validate()?;
        if self.budget_digest != self.execution.budget.digest()?
            || self.task_family != super::workflow_contract()["task_family"]
            || self
                .execution
                .review_of
                .as_ref()
                .is_some_and(|basis| basis.revision_ref != self.candidate_revision)
        {
            return Err(invalid(
                "execution contract disagrees with the trial identity",
            ));
        }
        if self.task_family.trim().is_empty()
            || self.task_family.len() > 256
            || self.observation_window.trim().is_empty()
            || self.observation_window.len() > 256
        {
            return Err(invalid(
                "paired trial requires bounded family and observation window",
            ));
        }
        if !is_revision(&self.candidate_revision) {
            return Err(invalid(
                "paired trial requires an immutable candidate revision reference",
            ));
        }
        for digest in [
            &self.environment_digest,
            &self.base_memory_digest,
            &self.model_digest,
            &self.budget_digest,
        ] {
            if !is_digest(digest) {
                return Err(invalid(
                    "paired trial identities require canonical SHA-256 digests",
                ));
            }
        }
        if self.tool_versions.is_empty()
            || self.tool_versions.len() > 64
            || self.tool_versions.iter().any(|(k, v)| {
                k.trim().is_empty() || k.len() > 256 || v.trim().is_empty() || v.len() > 256
            })
        {
            return Err(invalid(
                "paired trial requires bounded, explicit tool versions",
            ));
        }
        if !(2..=MAX_PAIRS).contains(&self.pairs.len()) {
            return Err(invalid(
                "paired trial requires between 2 and 4096 predeclared pairs",
            ));
        }
        let mut cases = BTreeSet::new();
        for (id, case) in &self.pairs {
            if id.trim().is_empty()
                || id.len() > 128
                || !is_digest(&case.task_digest)
                || !is_digest(&case.initial_state_digest)
                || case.seed.trim().is_empty()
                || case.seed.len() > 128
                || !cases.insert((&case.task_digest, &case.initial_state_digest, &case.seed))
            {
                return Err(invalid(
                    "pair identities must be bounded, unique and digest-pinned",
                ));
            }
        }
        if !self.alpha.is_finite()
            || self.alpha <= 0.0
            || self.alpha >= 1.0
            || !self.minimum_effect.is_finite()
            || self.minimum_effect <= 0.0
            || self.minimum_effect > 1.0
        {
            return Err(invalid(
                "paired alpha must be in (0,1), minimum_effect in (0,1]",
            ));
        }
        Ok(())
    }

    pub fn artifact(&self) -> Result<Json, KipError> {
        self.validate()?;
        serde_json::to_value(self).map_err(|e| invalid(e.to_string()))
    }

    /// Computes the expected pin; this does not store or authorize an artifact.
    pub fn pin(&self) -> Result<ArtifactPin, KipError> {
        let content_digest = content_digest(&self.artifact()?)?;
        Ok(ArtifactPin {
            artifact_ref: format!("kip:artifact:{content_digest}"),
            content_digest,
        })
    }

    /// The immutable host context attached before an attempt executes.
    /// Arm membership is read from the actual Trial/Attempt bindings by the
    /// evaluator, never inferred from a model-written `arm` label.
    pub fn attempt_context(&self, pair_id: &str) -> Result<Json, KipError> {
        self.validate()?;
        self.validated_attempt_context(pair_id)
    }

    pub(super) fn validated_attempt_context(&self, pair_id: &str) -> Result<Json, KipError> {
        let case = self
            .pairs
            .get(pair_id)
            .ok_or_else(|| invalid("pair was not enrolled"))?;
        Ok(json!({
            "pair_id": pair_id,
            "task_family": self.task_family,
            "task_digest": case.task_digest,
            "initial_state_digest": case.initial_state_digest,
            "seed": case.seed,
            "base_memory_digest": self.base_memory_digest,
            "model_digest": self.model_digest,
            "budget_digest": self.budget_digest,
            "stratum": "all"
        }))
    }

    /// KIP's closed comparability object: extra pins live in the parameter
    /// artifact and AttemptRecord.context, not invented standard fields.
    pub fn comparability(&self, observer_control_digest: &str) -> Result<Json, KipError> {
        self.validate()?;
        if !is_digest(observer_control_digest) {
            return Err(invalid("invalid observer-control digest"));
        }
        Ok(json!({
            "method": "paired",
            "environment_digest": self.environment_digest,
            "strata_weights": {"all": 1.0},
            "metric": "success",
            "minimum_effect": self.minimum_effect,
            "uncertainty_rule": "paired-hoeffding-v1",
            "missingness_policy": "count_as_failure",
            "observer_control_digest": observer_control_digest,
            "sampling_unit": "pair_id",
            "correlation_policy": "independent_pairs"
        }))
    }

    pub fn treatment_revisions(&self) -> Vec<String> {
        vec![self.candidate_revision.clone()]
    }
}
