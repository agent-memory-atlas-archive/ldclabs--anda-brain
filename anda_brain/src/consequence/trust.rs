//! Optional, scoped source calibration. Facts, operational outcomes, and
//! authority are separate inputs; no cognition can install this runtime.
use anda_cognitive_nexus::{attention::RuntimeScope, content_digest, trust::ContextualTrustRule};
use anda_core::{BoxError, Json};
use anda_kip::cognitive::{ArtifactPin, ObserverControl};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

mod evidence;
mod governance;
mod proposal;
mod runtime;
pub use runtime::TrustRuntime;

pub const FORMAT: &str = "anda-brain:contextual-trust-v1";
const VERIFICATION: &str = "anda-brain:fact-verification-v1";

#[derive(Debug)]
struct Ineligible(&'static str);
impl std::fmt::Display for Ineligible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Ineligible {}
fn ineligible(reason: &'static str) -> BoxError {
    Box::new(Ineligible(reason))
}

/// Registered method: binary factual accuracy in ONE exact predicate/context.
/// Predictive probability calibration and global actor updates are unsupported.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustConfig {
    pub version: String,
    pub proposer_principal: String,
    pub governor_principal: Option<String>,
    pub observer: ObserverControl,
    pub predicate_ref: String,
    pub context_ref: String,
    pub task_family: String,
    pub environment_digest: String,
    pub parameters: Option<TrustParameters>,
    pub calibration: Option<TrustCalibration>,
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub automatic_apply: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustParameters {
    pub minimum_samples: usize,
    pub alpha: f64,
    pub gain: f64,
    pub step_cap: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustCalibration {
    pub reviewed_by: String,
    pub contract_digest: String,
    pub approved: bool,
    pub material: Json,
}
impl TrustConfig {
    pub fn contract_digest(&self) -> Result<String, BoxError> {
        Ok(content_digest(
            &json!({"format":FORMAT,"version":self.version,"method":"binary-fact-accuracy-v1",
            "observer":self.observer,"predicate":self.predicate_ref,"context":self.context_ref,
            "task_family":self.task_family,"environment":self.environment_digest,"parameters":self.parameters,
            "selection":"first-n-native-creation-seq; unique-unconsumed-instrument-roots; verified-facts-only",
            "uncertainty":"two-sided-hoeffding-v1","mapping":"clamp(old+clip(gain*(accuracy-old),+-step_cap),0,1)",
            "application":"exact-actor-predicate-context; preserve-global-and-other-rules; explicit-version-CAS"}),
        )?)
    }
    pub fn calibrated(&self) -> bool {
        self.parameters.is_some() && self.calibration.as_ref().is_some_and(|c| c.approved)
    }
    pub fn validate(&self) -> Result<(), BoxError> {
        use crate::runtime_api::{digest_valid, principal_valid};
        concept(&self.context_ref)?;
        if self.version.trim().is_empty()
            || self.version.len() > 128
            || !principal_valid(&self.proposer_principal)
            || !principal_valid(&self.observer.principal_id)
            || self.proposer_principal == self.observer.principal_id
            || self.governor_principal.as_ref().is_some_and(|p| {
                !principal_valid(p)
                    || p == &self.proposer_principal
                    || p == &self.observer.principal_id
            })
            || !digest_valid(&self.observer.configuration_digest)
            || !digest_valid(&self.environment_digest)
            || self.observer.control_domain.is_empty()
            || self.observer.control_domain.len() > 256
            || !self.predicate_ref.starts_with("kip://")
            || self.predicate_ref.len() > 512
            || self.task_family.is_empty()
            || self.task_family.len() > 256
        {
            return Err("trust requires distinct registered principals and an exact predicate/context domain".into());
        }
        if let Some(p) = &self.parameters
            && (!(1..=32).contains(&p.minimum_samples)
                || !p.alpha.is_finite()
                || !(0.0..1.0).contains(&p.alpha)
                || p.alpha == 0.0
                || [p.gain, p.step_cap]
                    .iter()
                    .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v) || *v == 0.0))
        {
            return Err("invalid explicit trust calibration parameters".into());
        }
        if let Some(c) = &self.calibration
            && (!principal_valid(&c.reviewed_by)
                || c.reviewed_by == self.observer.principal_id
                || c.reviewed_by == self.proposer_principal
                || c.contract_digest != self.contract_digest()?
                || !c.material.is_object()
                || c.material.as_object().is_none_or(|m| m.is_empty())
                || serde_json::to_vec(&c.material)?.len() > 16_384)
        {
            return Err("trust method review must approve the exact registered contract".into());
        }
        if self.apply && (!self.calibrated() || self.governor_principal.is_none()) {
            return Err(
                "trust application needs reviewed calibration and a separately authorized governor"
                    .into(),
            );
        }
        if self.automatic_apply && (!self.automatic || !self.apply) {
            return Err("automatic trust application requires both explicit switches".into());
        }
        Ok(())
    }
}
fn concept(id: &str) -> Result<(), BoxError> {
    let parsed = anda_cognitive_nexus::ElementId::parse_kind(id, anda_kip::ElementKind::Concept)?;
    if parsed.to_string() != id {
        return Err("canonical Concept reference required".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCause {
    VerifiedFact,
    ExecutionFailure,
    MissingPrecondition,
    EnvironmentChange,
    Unknown,
}

/// Submitted only by a directly authenticated independent instrument. `material`
/// contains its replayable factual comparison, not an agent's success report.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustVerificationInput {
    pub event_key: String,
    /// Shared by summaries/copies/correlated observations from the same root.
    pub root_key: String,
    pub assertion_ref: String,
    pub assessed_at: String,
    pub cause: VerificationCause,
    pub correct: Option<bool>,
    pub material: Json,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationRecord {
    format: String,
    scope: RuntimeScope,
    contract_digest: String,
    observer: ObserverControl,
    input: TrustVerificationInput,
    actor_ref: String,
    proposition_ref: String,
    claim_digest: String,
    source_principal: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustProposal {
    pub format: String,
    pub id: String,
    pub scope: RuntimeScope,
    pub contract_digest: String,
    pub actor_ref: String,
    pub predicate_ref: String,
    pub context_ref: String,
    pub expected_version: u64,
    pub previous_rule: Option<ContextualTrustRule>,
    pub proposed_rule: Option<ContextualTrustRule>,
    pub old_weight: f64,
    pub new_weight: Option<f64>,
    pub independent_samples: usize,
    pub evidence_refs: Vec<String>,
    pub roots: Vec<String>,
    pub excluded: BTreeMap<String, String>,
    pub uncertainty: Json,
    pub method: ArtifactPin,
    pub native_proposal: Option<ArtifactPin>,
    pub reason: Option<String>,
    pub restores: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustReceipt {
    pub proposal_id: String,
    pub reviewed_proposal: ArtifactPin,
    pub governor: String,
    pub version: u64,
    pub space_seq: u64,
    pub control_ref: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustStatus {
    pub configured: bool,
    pub automatic: bool,
    pub apply: bool,
    pub automatic_apply: bool,
    pub calibrated: bool,
    pub governor_authorized: bool,
    pub running: bool,
    pub reason: Option<String>,
}
