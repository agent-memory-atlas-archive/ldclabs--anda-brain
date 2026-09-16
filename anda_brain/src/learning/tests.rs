use super::*;
use anda_cognitive_nexus::{CognitiveNexus, content_digest, nexus::DEFAULT_SPACE};
use anda_db::database::AndaDB;
use anda_kip::{
    Json,
    cognitive::{EvaluationInput, EvaluationRule, EvaluationSamples},
};
use object_store::memory::InMemory;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

fn digest(label: &str) -> String {
    content_digest(&json!({"identity":label})).unwrap()
}

pub(super) fn plan(n: usize) -> PairedTrialPlan {
    let budget = AttemptBudget {
        tool_calls: 4,
        elapsed_ms: 10_000,
        input_tokens: 4096,
        output_tokens: 1024,
    };
    PairedTrialPlan {
        task_family: "tool_workflow.precondition.v1".into(),
        candidate_revision: "C-2".into(),
        environment_digest: digest("environment"),
        base_memory_digest: digest("same-factual-memory"),
        model_digest: digest("same-business-model"),
        budget_digest: budget.digest().unwrap(),
        tool_versions: BTreeMap::from([("workflow".into(), "1".into())]),
        observation_window: "first-attempt-within-budget".into(),
        pairs: (0..n)
            .map(|i| {
                (
                    format!("pair-{i:04}"),
                    PairCase {
                        task_digest: digest(&format!("task-{i}")),
                        initial_state_digest: digest(&format!("state-{i}")),
                        seed: i.to_string(),
                    },
                )
            })
            .collect(),
        alpha: 0.05,
        minimum_effect: 0.1,
        execution: ExecutionContract {
            workflow: super::execution::pin(&workflow_contract()).unwrap(),
            budget,
            cutoff: "2099-01-01T00:00:00.000Z".into(),
            review_due_at: "2099-01-02T00:00:00.000Z".into(),
            maximum_failure_rate: 0.25,
            review_of: None,
        },
    }
}

fn fixture(n: usize, baseline_success: bool, candidate_success: bool) -> EvaluationInput {
    let p = plan(n);
    let mut attempts = serde_json::Map::new();
    let mut outcomes = serde_json::Map::new();
    let mut baseline_refs = Vec::new();
    for (i, pair_id) in p.pairs.keys().enumerate() {
        for (arm, success) in [(0, baseline_success), (1, candidate_success)] {
            let reference = format!("X-{}", 1 + i * 2 + arm);
            if arm == 0 {
                baseline_refs.push(reference.clone());
            }
            let revisions = if arm == 0 {
                Vec::new()
            } else {
                p.treatment_revisions()
            };
            attempts.insert(
                reference.clone(),
                json!({"principal_id":"actor","record":{
                    "attempt_id":format!("attempt-{i}-{arm}"),
                    "context":p.attempt_context(pair_id).unwrap(),
                    "selection_policy":p.pin().unwrap(),
                    "environment_digest":p.environment_digest,
                    "tool_versions":p.tool_versions,
                    "preconditions_satisfied":"yes",
                    "applied_revisions":revisions,
                    "trial_ref":if arm == 0 { Json::Null } else { json!("X-99999") }
                }}),
            );
            outcomes.insert(format!("E-{}", 1 + i * 2 + arm), json!({
                "principal_id":"independent-instrument", "status":"active", "corrected_by":[],
                "record":{"task_family":p.task_family,"attempt_ref":reference,"terminal":true,"metric":"success",
                    "observer_config_digest":p.execution.observer_configuration_digest().unwrap(),
                    "window":p.observation_window,"outcome_status":if success {"success"} else {"failure"}}
            }));
        }
    }
    EvaluationInput {
        rule: paired_rule_artifact(),
        parameters: p.artifact().unwrap(),
        trial: json!({"parameters":p.pin().unwrap(),"comparability":p.comparability(&digest("observers")).unwrap(),
            "revision_refs":p.treatment_revisions(),"quota":n,"observation_window":p.observation_window,
            "baseline_attempt_refs":baseline_refs}),
        attempts: Json::Object(attempts),
        outcomes: Json::Object(outcomes),
        samples: EvaluationSamples::default(),
        minimum_independent_attempts: 2,
    }
}

#[test]
fn paired_rule_requires_incremental_improvement_and_uncertainty() {
    let improved = PairedRule.evaluate(&fixture(64, false, true)).unwrap();
    assert_eq!(improved["status"], "improved");
    assert_eq!(improved["effect"], 1.0);
    assert!(improved["uncertainty"]["lower_bound"].as_f64().unwrap() > 0.1);
    assert_eq!(
        PairedRule.evaluate(&fixture(64, true, true)).unwrap()["status"],
        "not_improved"
    );
    assert_eq!(
        PairedRule.evaluate(&fixture(64, true, false)).unwrap()["effect"],
        -1.0
    );
    // A perfect outcome on two pairs cannot clear this uncertainty bound.
    assert_eq!(
        PairedRule.evaluate(&fixture(2, false, true)).unwrap()["status"],
        "not_improved"
    );
}

#[test]
fn fixed_cohort_prevents_early_stopping_and_cherry_picking() {
    let mut input = fixture(64, false, true);
    input.attempts.as_object_mut().unwrap().remove("X-2");
    input.outcomes.as_object_mut().unwrap().remove("E-2");
    assert_eq!(
        PairedRule.evaluate(&input).unwrap()["status"],
        "insufficient"
    );

    input.attempts.as_object_mut().unwrap().remove("X-1");
    input.outcomes.as_object_mut().unwrap().remove("E-1");
    assert!(
        PairedRule.evaluate(&input).is_err(),
        "baseline cannot be trimmed after enrollment"
    );
}

#[test]
fn missing_treatment_is_failure_missing_baseline_cannot_invent_gain() {
    let mut input = fixture(64, false, true);
    input.outcomes.as_object_mut().unwrap().remove("E-2");
    assert_eq!(PairedRule.evaluate(&input).unwrap()["effect"], 63.0 / 64.0);
    input.outcomes.as_object_mut().unwrap().remove("E-1");
    assert_eq!(
        PairedRule.evaluate(&input).unwrap()["status"],
        "insufficient"
    );
    assert!(PairedRule.evaluate(&input).unwrap()["effect"].is_null());

    let mut input = fixture(8, false, true);
    input.outcomes["E-1"]["record"]["outcome_status"] = json!("unknown");
    assert_eq!(
        PairedRule.evaluate(&input).unwrap()["status"],
        "insufficient"
    );
    let mut input = fixture(8, false, true);
    input.outcomes["E-2"]["record"]["outcome_status"] = json!("unknown");
    assert_eq!(PairedRule.evaluate(&input).unwrap()["effect"], 7.0 / 8.0);
}

#[test]
fn wrong_family_and_nonterminal_or_corrected_outcomes_are_rejected() {
    let mut partial = fixture(8, false, true);
    partial.outcomes["E-2"]["record"]["outcome_status"] = json!("partial");
    partial.outcomes["E-2"]["record"]["magnitude"] = json!(1.0);
    assert!(
        PairedRule.evaluate(&partial).is_err(),
        "a partial receipt cannot masquerade as complete workflow success"
    );
    for (field, value) in [
        ("task_family", json!("another-family")),
        ("metric", json!("latency")),
        ("window", json!("another-window")),
        ("terminal", json!(false)),
        ("observer_config_digest", json!(digest("wrong-instrument"))),
    ] {
        let mut input = fixture(8, false, true);
        input.outcomes["E-2"]["record"][field] = value;
        assert!(PairedRule.evaluate(&input).is_err(), "{field}");
    }
    let mut input = fixture(8, false, true);
    input.outcomes["E-2"]["corrected_by"] = json!(["E-999"]);
    assert!(PairedRule.evaluate(&input).is_err());
}

#[test]
fn changed_pair_pins_and_cross_trial_attempts_are_rejected() {
    for field in [
        "task_digest",
        "initial_state_digest",
        "seed",
        "base_memory_digest",
        "model_digest",
        "budget_digest",
        "task_family",
        "pair_id",
        "stratum",
    ] {
        let mut input = fixture(8, false, true);
        input.attempts["X-2"]["record"]["context"][field] = json!("changed");
        assert!(PairedRule.evaluate(&input).is_err(), "{field}");
    }
    for (field, value) in [
        ("environment_digest", json!(digest("changed"))),
        ("tool_versions", json!({"workflow":"2"})),
        ("applied_revisions", json!(["C-3"])),
        ("trial_ref", json!("X-100000")),
        (
            "selection_policy",
            json!({"content_digest":digest("changed")}),
        ),
    ] {
        let mut input = fixture(8, false, true);
        input.attempts["X-2"]["record"][field] = value;
        assert!(PairedRule.evaluate(&input).is_err(), "{field}");
    }
}

#[test]
fn duplicate_observations_do_not_create_independent_successes() {
    let mut input = fixture(8, false, true);
    input.outcomes["E-999"] = input.outcomes["E-2"].clone();
    assert!(PairedRule.evaluate(&input).is_err());
    let mut input = fixture(8, false, true);
    input.attempts["X-999"] = input.attempts["X-2"].clone();
    assert!(PairedRule.evaluate(&input).is_err());
    let mut input = fixture(8, false, true);
    input.attempts["X-1"]["record"]["applied_revisions"] = json!(["C-2"]);
    assert!(
        PairedRule.evaluate(&input).is_err(),
        "control must not apply the candidate"
    );
}

#[test]
fn frozen_parameters_and_rule_cannot_be_silently_replaced() {
    let mut legacy = fixture(8, false, true);
    legacy.rule = json!({"engine":"anda-brain:paired-bounded-v1"});
    assert!(
        PairedRule.evaluate(&legacy).is_err(),
        "the old rule digest cannot acquire new budget/failure semantics"
    );
    let mut input = fixture(8, false, true);
    input.parameters["minimum_effect"] = json!(0.001);
    assert!(PairedRule.evaluate(&input).is_err());
    let mut input = fixture(8, false, true);
    input.rule = json!({"engine":"kip:binary-stratified-v1"});
    assert!(PairedRule.evaluate(&input).is_err());
    let mut input = fixture(8, false, true);
    input.minimum_independent_attempts = 100;
    assert_eq!(
        PairedRule.evaluate(&input).unwrap()["status"],
        "insufficient"
    );
}

#[test]
fn manifest_rejects_unbounded_or_duplicate_experiments() {
    let mut p = plan(8);
    p.pairs
        .insert("duplicate".into(), p.pairs.values().next().unwrap().clone());
    assert!(p.validate().is_err());
    for alpha in [0.0, 1.0, f64::NAN, f64::INFINITY] {
        let mut p = plan(8);
        p.alpha = alpha;
        assert!(p.validate().is_err());
    }
    let mut p = plan(8);
    p.minimum_effect = 0.0;
    assert!(p.validate().is_err());
    let mut p = plan(8);
    p.model_digest = "unknown".into();
    assert!(p.validate().is_err());
    assert!(plan(1).validate().is_err());
    let mut artifact = plan(8).artifact().unwrap();
    artifact["incumbent_revisions"] = json!(["C-3"]);
    assert!(
        serde_json::from_value::<PairedTrialPlan>(artifact).is_err(),
        "v1 cannot silently ignore extra applied Skills"
    );
}

#[test]
fn execution_contract_freezes_budget_cutoff_review_and_absolute_failure_gate() {
    let p = plan(64);
    assert!(
        p.execution
            .validate_settlement(&p.execution.cutoff, "2098-12-31T23:59:59.999Z")
            .is_err()
    );
    assert!(
        p.execution
            .validate_settlement("2099-01-01T00:00:01.000Z", "2099-01-02T00:00:00.000Z")
            .is_err()
    );
    assert!(
        p.execution
            .validate_settlement(&p.execution.cutoff, "2099-01-02T00:00:00.000Z")
            .is_ok()
    );
    let mut changed = p.clone();
    changed.execution.budget.tool_calls += 1;
    assert!(
        changed.validate().is_err(),
        "a changed budget must change its identity"
    );
    let mut changed = p.clone();
    changed.execution.review_due_at = p.execution.cutoff.clone();
    assert!(changed.validate().is_err());
    let mut changed = p.clone();
    changed.execution.review_of = Some(AdoptionBasis {
        revision_ref: "C-99".into(),
        trial_ref: "X-1".into(),
        evaluation_ref: "X-2".into(),
    });
    assert!(changed.validate().is_err());
    assert_eq!(workflow_contract()["task_family"], p.task_family);

    let mut input = fixture(64, false, true);
    // Still a large comparative gain, but one failure breaches a frozen zero
    // failure ceiling. Passing the interval alone cannot confer improvement.
    let mut strict = p;
    strict.execution.maximum_failure_rate = 0.0;
    input.parameters = strict.artifact().unwrap();
    input.trial["parameters"] = json!(strict.pin().unwrap());
    for wrapped in input.attempts.as_object_mut().unwrap().values_mut() {
        wrapped["record"]["selection_policy"] = json!(strict.pin().unwrap());
    }
    input.outcomes["E-2"]["record"]["outcome_status"] = json!("failure");
    let result = PairedRule.evaluate(&input).unwrap();
    assert!(result["uncertainty"]["lower_bound"].as_f64().unwrap() > 0.1);
    assert_eq!(result["status"], "not_improved");
}

#[tokio::test]
async fn manifest_is_persisted_and_rule_registration_is_restored_explicitly() {
    let db = Arc::new(
        AndaDB::connect(
            Arc::new(InMemory::new()),
            crate::testkit::db_config("paired_rule_registry"),
        )
        .await
        .unwrap(),
    );
    let nexus = CognitiveNexus::connect(db.clone()).await.unwrap();
    let p = plan(8);
    let pin = nexus
        .system_session()
        .put_artifact(DEFAULT_SPACE, p.artifact().unwrap(), vec![])
        .await
        .unwrap();
    assert_eq!(pin, p.pin().unwrap());
    let rule_digest = register_paired_rule(&nexus).unwrap();
    assert_eq!(
        rule_digest,
        content_digest(&paired_rule_artifact()).unwrap()
    );
    assert!(
        register_paired_rule(&nexus).is_err(),
        "a digest cannot be rebound"
    );
    assert_eq!(
        nexus
            .store
            .evaluation_rules
            .evaluate(&fixture(8, false, true))
            .unwrap()["status"],
        "improved"
    );
    let restarted = CognitiveNexus::connect(db.clone()).await.unwrap();
    assert_eq!(
        restarted
            .system_session()
            .read_artifact(DEFAULT_SPACE, &pin)
            .await
            .unwrap(),
        p.artifact().unwrap()
    );
    assert!(
        restarted
            .store
            .evaluation_rules
            .evaluate(&fixture(8, false, true))
            .is_err(),
        "stored artifact cannot execute code"
    );
    register_paired_rule(&restarted).unwrap();
    assert_eq!(
        restarted
            .store
            .evaluation_rules
            .evaluate(&fixture(8, false, true))
            .unwrap()["status"],
        "improved"
    );
    db.close().await.unwrap();
}
