use anda_cognitive_nexus::CognitiveNexus;
use anda_kip::{
    Json, KipError,
    cognitive::{EvaluationInput, EvaluationRule},
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::{PairedTrialPlan, invalid, plan::MAX_PAIRS};

/// Versioned binding for trusted code. Algorithm changes require a new engine
/// identity; stored rule bytes alone never register or execute the evaluator.
pub fn paired_rule_artifact() -> Json {
    json!({"engine": "anda-brain:paired-bounded-v2"})
}

/// Call once per Nexus instance, including after restart. Duplicate
/// registration is refused by Nexus rather than silently rebinding code.
pub fn register_paired_rule(nexus: &CognitiveNexus) -> Result<String, KipError> {
    nexus.register_evaluation_rule(&paired_rule_artifact(), Arc::new(PairedRule))
}

/// Evaluates engine-validated material. Nexus owns observer authorization,
/// original record identity, temporal eligibility, replay and atomic standing.
/// This rule adds the frozen cohort, exact paired identities and incremental
/// comparison. Calling it directly computes a number, never grants standing.
pub struct PairedRule;

impl EvaluationRule for PairedRule {
    fn evaluate(&self, input: &EvaluationInput) -> Result<Json, KipError> {
        if input.rule != paired_rule_artifact() {
            return Err(KipError::unsupported_capability(
                "unsupported paired evaluator version",
            ));
        }
        let plan: PairedTrialPlan = serde_json::from_value(input.parameters.clone())
            .map_err(|e| invalid(format!("invalid paired plan: {e}")))?;
        plan.validate()?;
        let comparability = &input.trial["comparability"];
        let expected = plan.comparability(
            comparability["observer_control_digest"]
                .as_str()
                .unwrap_or(""),
        )?;
        // KML may render integral numbers as 1 where serde emitted 1.0.
        // Compare canonical KIP content, as the engine does for replay.
        if anda_kip::canonical_json(comparability) != anda_kip::canonical_json(&expected)
            || input.trial["parameters"]
                != serde_json::to_value(plan.pin()?).map_err(|e| invalid(e.to_string()))?
            || input.trial["observation_window"] != plan.observation_window
            || refs(&input.trial["revision_refs"])?
                != plan.treatment_revisions().into_iter().collect()
            || input.trial["quota"].as_u64() != Some(plan.pairs.len() as u64)
        {
            return Err(invalid("trial disagrees with the frozen paired plan"));
        }
        let baseline = refs(&input.trial["baseline_attempt_refs"])?;
        let attempts = input
            .attempts
            .as_object()
            .ok_or_else(|| invalid("attempt replay must be an object"))?;
        let outcomes = input
            .outcomes
            .as_object()
            .ok_or_else(|| invalid("outcome replay must be an object"))?;
        if attempts.len() > 2 * MAX_PAIRS || outcomes.len() > 2 * MAX_PAIRS {
            return Err(invalid("paired replay exceeds the bounded cohort"));
        }
        if baseline.len() != plan.pairs.len() || !baseline.iter().all(|r| attempts.contains_key(r))
        {
            return Err(invalid(
                "the entire baseline cohort must be frozen before trial entry",
            ));
        }
        let selection = serde_json::to_value(plan.pin()?).map_err(|e| invalid(e.to_string()))?;
        let mut controls = BTreeMap::new();
        let mut treatments = BTreeMap::new();
        let mut trial_ref: Option<&Json> = None;
        for (reference, wrapped) in attempts {
            let record = &wrapped["record"];
            let pair_id = record["context"]["pair_id"]
                .as_str()
                .ok_or_else(|| invalid("attempt has no pair_id"))?;
            let context = plan.validated_attempt_context(pair_id)?;
            // Additional context is allowed by KIP, but every comparison pin
            // must match the predeclared host manifest exactly.
            if context
                .as_object()
                .unwrap()
                .iter()
                .any(|(k, v)| record["context"].get(k) != Some(v))
                || record["selection_policy"] != selection
                || record["environment_digest"] != plan.environment_digest
                || record["tool_versions"] != json!(plan.tool_versions)
                || record["preconditions_satisfied"] != "yes"
            {
                return Err(invalid(
                    "attempt changed a pinned task, state, memory, model, budget or tool",
                ));
            }
            let is_baseline = baseline.contains(reference);
            let revisions = refs(&record["applied_revisions"])?;
            let expected_revisions = if is_baseline {
                BTreeSet::new()
            } else {
                plan.treatment_revisions().into_iter().collect()
            };
            if revisions != expected_revisions {
                return Err(invalid(
                    "control and treatment may differ only by the candidate revision",
                ));
            }
            if !is_baseline {
                let current = &record["trial_ref"];
                if current.as_str().is_none_or(str::is_empty)
                    || trial_ref.is_some_and(|r| r != current)
                {
                    return Err(invalid(
                        "treatment attempts must belong to one assigned trial",
                    ));
                }
                trial_ref = Some(current);
            }
            let into = if is_baseline {
                &mut controls
            } else {
                &mut treatments
            };
            if into.insert(pair_id, reference.as_str()).is_some() {
                return Err(invalid("a pair cannot be counted twice within an arm"));
            }
        }
        let mut values = BTreeMap::new();
        let mut unknown_controls = BTreeSet::new();
        let observer_configuration_digest = plan.execution.observer_configuration_digest()?;
        for wrapped in outcomes.values() {
            let record = &wrapped["record"];
            let reference = record["attempt_ref"]
                .as_str()
                .ok_or_else(|| invalid("unlinked outcome"))?;
            if !attempts.contains_key(reference)
                || record["terminal"] != true
                || record["metric"] != "success"
                || record["task_family"] != plan.task_family
                || record["observer_config_digest"] != observer_configuration_digest
                || record["window"] != plan.observation_window
                || wrapped["status"] == "corrected"
                || wrapped["corrected_by"]
                    .as_array()
                    .is_none_or(|v| !v.is_empty())
            {
                return Err(invalid("paired rule needs eligible terminal outcomes"));
            }
            let value: f64 = match record["outcome_status"].as_str() {
                Some("success") => 1.0,
                Some("partial") => {
                    return Err(invalid(
                        "this workflow requires binary bounded-success outcomes",
                    ));
                }
                Some("failure" | "aborted" | "unknown") => 0.0,
                _ => return Err(invalid("unsupported outcome status")),
            };
            if baseline.contains(reference) && record["outcome_status"] == "unknown" {
                unknown_controls.insert(reference);
            }
            if !value.is_finite()
                || !(0.0..=1.0).contains(&value)
                || values.insert(reference, value).is_some()
            {
                return Err(invalid("outcomes must be bounded and unique per attempt"));
            }
        }
        // A missing/unknown baseline cannot create an artificial gain.
        // Missing treatment outcomes count as zero once their attempt exists.
        let baseline_observed = controls.values().all(|r| values.contains_key(r));
        if treatments.len() != plan.pairs.len()
            || !baseline_observed
            || !unknown_controls.is_empty()
            || (treatments.len() as u64) < input.minimum_independent_attempts
        {
            return Ok(comparison(
                "insufficient",
                None,
                None,
                None,
                &plan,
                treatments.len(),
            ));
        }
        let mut effect = 0.0;
        let mut failures = 0usize;
        for pair in plan.pairs.keys() {
            let control = controls
                .get(pair.as_str())
                .ok_or_else(|| invalid("baseline pair omitted"))?;
            let treatment = treatments
                .get(pair.as_str())
                .ok_or_else(|| invalid("treatment pair omitted"))?;
            let value = values.get(treatment).copied().unwrap_or(0.0);
            effect += value - values[control];
            failures += usize::from(value < 1.0);
        }
        let n = plan.pairs.len();
        effect /= n as f64;
        // One-sided Hoeffding for independent paired differences in [-1,1].
        let radius = (2.0 * -plan.alpha.ln() / n as f64).sqrt();
        // Round the lower bound down before both the verdict and public
        // rendering, so serialization cannot imply a stronger bound.
        let lower = ((effect - radius) * 1e12).floor() / 1e12;
        let failure_rate = failures as f64 / n as f64;
        let mut result = comparison(
            if lower >= plan.minimum_effect && failure_rate <= plan.execution.maximum_failure_rate {
                "improved"
            } else {
                "not_improved"
            },
            Some(effect),
            Some(lower),
            Some(radius),
            &plan,
            n,
        );
        result["uncertainty"]["failure_rate"] = json!(failure_rate);
        result["uncertainty"]["maximum_failure_rate"] = json!(plan.execution.maximum_failure_rate);
        Ok(result)
    }
}

fn refs(value: &Json) -> Result<BTreeSet<String>, KipError> {
    let values = value
        .as_array()
        .ok_or_else(|| invalid("expected reference array"))?;
    let mut refs = BTreeSet::new();
    for value in values {
        let value = value
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid("invalid reference"))?;
        if !refs.insert(value.to_string()) {
            return Err(invalid("duplicate reference"));
        }
    }
    Ok(refs)
}

fn comparison(
    status: &str,
    effect: Option<f64>,
    lower: Option<f64>,
    radius: Option<f64>,
    plan: &PairedTrialPlan,
    pairs: usize,
) -> Json {
    let round = |v: f64| (v * 1e12).round() / 1e12;
    json!({"status":status,"effect":effect.map(round),"uncertainty":{
        "method":"paired-hoeffding-v1","alpha":plan.alpha,"lower_bound":lower.map(round),
        "radius":radius.map(round),"pairs":pairs,"planned_pairs":plan.pairs.len()
    }})
}
