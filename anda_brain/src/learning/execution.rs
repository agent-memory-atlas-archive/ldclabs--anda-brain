use anda_cognitive_nexus::{content_digest, time::normalize};
use anda_kip::{Json, KipError, cognitive::ArtifactPin};
use serde::{Deserialize, Serialize};

use super::{invalid, is_revision};

/// Frozen semantics for the first resettable task family. This is a contract,
/// not a deployed executor or an MIB capability declaration.
pub fn workflow_contract() -> Json {
    serde_json::from_str(include_str!(
        "../../assets/learning/workflow-contract-v1.json"
    ))
    .expect("checked-in workflow contract must be valid JSON")
}

pub(super) fn pin(value: &Json) -> Result<ArtifactPin, KipError> {
    let content_digest = content_digest(value)?;
    Ok(ArtifactPin {
        artifact_ref: format!("kip:artifact:{content_digest}"),
        content_digest,
    })
}

/// An absolute per-attempt ceiling, including failed calls and retries.
/// A verifier emits success only when every ceiling is satisfied. Unknown
/// usage cannot be reported as zero or successful bounded execution.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttemptBudget {
    pub tool_calls: u32,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl AttemptBudget {
    pub fn digest(&self) -> Result<String, KipError> {
        if self.tool_calls == 0
            || self.tool_calls > 64
            || self.elapsed_ms == 0
            || self.elapsed_ms > 3_600_000
            || self.input_tokens == 0
            || self.output_tokens == 0
            || self.input_tokens > 1_000_000
            || self.output_tokens > 1_000_000
        {
            return Err(invalid("attempt budgets must be explicit and bounded"));
        }
        content_digest(&serde_json::to_value(self).map_err(|e| invalid(e.to_string()))?)
    }
}

/// Retained acquisition evidence for a *new* monitoring trial. The host must
/// resolve these records, verify the revision/standing and put them in the
/// parameter artifact's source_refs. A string alone proves no lineage.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdoptionBasis {
    pub revision_ref: String,
    pub trial_ref: String,
    pub evaluation_ref: String,
}

impl AdoptionBasis {
    /// Pass the authenticated host's reads of the referenced immutable
    /// records, never caller-supplied lookalikes. This checks the lineage;
    /// current Skill status, dependency validity and review expiry are a
    /// separate read-time applicability check.
    pub fn validate_records(&self, trial: &Json, evaluation: &Json) -> Result<(), KipError> {
        let revisions = serde_json::json!([self.revision_ref]);
        if trial["revision_refs"] != revisions
            || evaluation["revision_refs"] != revisions
            || evaluation["trial_ref"] != self.trial_ref
            || evaluation["from_status"] != "trialed"
            || evaluation["to_status"] != "adopted"
            || evaluation["comparison"]["status"] != "improved"
        {
            return Err(invalid(
                "monitoring basis must be the same revision's acquisition verdict",
            ));
        }
        Ok(())
    }

    pub fn source_refs(&self) -> Vec<String> {
        vec![
            self.revision_ref.clone(),
            self.trial_ref.clone(),
            self.evaluation_ref.clone(),
        ]
    }
}

/// Host contract pinned before either arm runs. The rule receives no
/// EvaluationRecord.cutoff in today's KIP EvaluationInput, so the host must
/// call validate_settlement before writing its verdict. Registration alone
/// cannot enforce the deadline or schedule a review.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContract {
    pub workflow: ArtifactPin,
    pub budget: AttemptBudget,
    pub cutoff: String,
    pub review_due_at: String,
    pub maximum_failure_rate: f64,
    pub review_of: Option<AdoptionBasis>,
}

impl ExecutionContract {
    /// Bind the authorized observer to the exact success/budget semantics.
    /// Credentials and hidden world state never belong in these public bytes.
    pub fn observer_configuration(&self) -> Result<Json, KipError> {
        self.validate()?;
        Ok(serde_json::json!({"verifier":"bounded-first-commit-v1",
            "workflow":self.workflow,"budget":self.budget}))
    }

    pub fn observer_configuration_digest(&self) -> Result<String, KipError> {
        content_digest(&self.observer_configuration()?)
    }

    pub fn validate(&self) -> Result<(), KipError> {
        if self.workflow != pin(&workflow_contract())? {
            return Err(invalid("unsupported workflow contract"));
        }
        self.budget.digest()?;
        let cutoff = normalize(&self.cutoff, "trial cutoff")?;
        let review = normalize(&self.review_due_at, "review deadline")?;
        if cutoff != self.cutoff || review != self.review_due_at || review <= cutoff {
            return Err(invalid("review must follow a canonical frozen cutoff"));
        }
        if !self.maximum_failure_rate.is_finite()
            || !(0.0..=1.0).contains(&self.maximum_failure_rate)
        {
            return Err(invalid("maximum failure rate must be in [0,1]"));
        }
        if let Some(basis) = &self.review_of {
            let activity_ref = |value: &str| {
                value.strip_prefix("X-").is_some_and(|id| {
                    id.parse::<u64>()
                        .is_ok_and(|n| n > 0 && n.to_string() == id)
                })
            };
            if !is_revision(&basis.revision_ref)
                || !activity_ref(&basis.trial_ref)
                || !activity_ref(&basis.evaluation_ref)
            {
                return Err(invalid(
                    "monitoring must retain native acquisition references",
                ));
            }
        }
        Ok(())
    }

    /// Fail closed before settlement; retries may replay the same frozen
    /// cutoff later, but must not move it or settle before it.
    pub fn validate_settlement(&self, cutoff: &str, now: &str) -> Result<(), KipError> {
        self.validate()?;
        if cutoff != self.cutoff || normalize(now, "settlement time")? < self.cutoff {
            return Err(invalid("settlement must use the reached, frozen cutoff"));
        }
        Ok(())
    }
}
