//! Protocol fixtures only: independent native records exercise settlement
//! contracts, not a live-model or external-executor performance claim.
use super::super::tests as fixture;
use super::*;
use anda_cognitive_nexus::nexus::DEFAULT_SPACE;
use anda_db::database::AndaDB;

struct Study {
    db: Arc<AndaDB>,
    #[cfg(feature = "experiments")]
    nexus: Arc<CognitiveNexus>,
    native: NativeLearning,
    input: NativeVerdictInput,
    #[cfg(feature = "experiments")]
    basis: Json,
}

async fn prepare_study(label: &str) -> Study {
    let (db, nexus, native, original, basis, _) = fixture::setup().await;
    let mut plan = super::super::super::tests::plan(8);
    plan.candidate_revision = original.candidate_revision;
    let frozen = install(&native, &plan, label).await;
    let trial_ref = baseline(&native, &plan, &frozen, &basis, label).await;
    #[cfg(not(feature = "experiments"))]
    drop(nexus);
    Study {
        db,
        #[cfg(feature = "experiments")]
        nexus,
        native,
        input: NativeVerdictInput {
            plan,
            frozen,
            trial_ref,
            idempotency_key: format!("{label}-verdict"),
        },
        #[cfg(feature = "experiments")]
        basis,
    }
}

async fn install(native: &NativeLearning, plan: &PairedTrialPlan, label: &str) -> FrozenNativePlan {
    native
        .execute(
            &NativeRequest {
                space_id: DEFAULT_SPACE.into(),
                idempotency_key: format!("{label}-install"),
                operation: NativeOperation::InstallPlan(InstallPlanInput {
                    plan: plan.clone(),
                    policy_id: format!("{label}-policy"),
                    policy_version: "1".into(),
                    expected_policy_version: 0,
                    allowed_parameters: vec![],
                }),
            },
            None,
        )
        .await
        .unwrap()
        .frozen_plan
        .unwrap()
}

fn attempt(
    plan: &PairedTrialPlan,
    basis: &Json,
    pair: &str,
    arm: NativeArm,
    label: &str,
) -> NativeRequest {
    let mut r = fixture::attempt_request(plan, basis, pair, arm);
    r.idempotency_key = format!("{label}-{}", r.idempotency_key);
    if let NativeOperation::Attempt(a) = &mut r.operation {
        a.attempt_id = r.idempotency_key.clone();
    }
    r
}

async fn baseline(
    native: &NativeLearning,
    plan: &PairedTrialPlan,
    frozen: &FrozenNativePlan,
    basis: &Json,
    label: &str,
) -> String {
    let mut attempts = vec![];
    let mut outcomes = vec![];
    for pair in plan.pairs.keys() {
        let a = native
            .execute(
                &attempt(plan, basis, pair, NativeArm::Baseline, label),
                None,
            )
            .await
            .unwrap();
        let mut o = fixture::outcome_request(plan, frozen, pair, NativeArm::Baseline, &a);
        if let NativeOperation::Outcome(input) = &mut o.operation {
            input.outcome_status = NativeOutcomeStatus::Failure;
        }
        let e = native
            .execute(
                &o,
                Some(&AuthContext::principal(
                    native.observer.principal_id.clone(),
                )),
            )
            .await
            .unwrap();
        attempts.push(a.handles["attempt"].clone());
        outcomes.push(e.handles["outcome"].clone());
    }
    let input = native
        .prepare_trial(
            plan.clone(),
            frozen.clone(),
            basis.clone(),
            attempts,
            outcomes,
        )
        .await
        .unwrap();
    native
        .execute(
            &NativeRequest {
                space_id: DEFAULT_SPACE.into(),
                idempotency_key: format!("{label}-trial"),
                operation: NativeOperation::OpenTrial(input),
            },
            None,
        )
        .await
        .unwrap()
        .handles["trial"]
        .clone()
}

async fn activate(study: &Study) {
    let now = anda_cognitive_nexus::time::now();
    let mut input = study.input.clone();
    input.idempotency_key.push_str("-activate");
    let prepared = study
        .native
        .prepare_activation(&input, &now)
        .await
        .unwrap()
        .unwrap();
    study.native.execute_verdict(&prepared, &now).await.unwrap();
}

#[cfg(feature = "experiments")]
async fn treatment(study: &Study, last_fails: bool) -> Option<(String, String)> {
    let mut failed = None;
    for (i, pair) in study.input.plan.pairs.keys().enumerate() {
        let arm = NativeArm::Treatment {
            trial_ref: study.input.trial_ref.clone(),
        };
        let a = study
            .native
            .execute(
                &attempt(
                    &study.input.plan,
                    &study.basis,
                    pair,
                    arm.clone(),
                    "treatment",
                ),
                None,
            )
            .await
            .unwrap();
        let mut o = fixture::outcome_request(&study.input.plan, &study.input.frozen, pair, arm, &a);
        if last_fails
            && i + 1 == study.input.plan.pairs.len()
            && let NativeOperation::Outcome(input) = &mut o.operation
        {
            input.outcome_status = NativeOutcomeStatus::Failure;
        }
        let e = study
            .native
            .execute(
                &o,
                Some(&AuthContext::principal(
                    study.native.observer.principal_id.clone(),
                )),
            )
            .await
            .unwrap();
        if last_fails && i + 1 == study.input.plan.pairs.len() {
            failed = Some((a.handles["attempt"].clone(), e.handles["outcome"].clone()));
        }
    }
    failed
}

async fn signal(study: &Study, key: &str) -> NativeSafetySignalReceipt {
    study
        .native
        .record_safety_signal(
            &NativeSafetySignalInput {
                revision_ref: study.input.plan.candidate_revision.clone(),
                observation_key: key.into(),
                observed_at: anda_cognitive_nexus::time::now(),
                journal_digest: content_digest(&json!({"independent_instrument":key})).unwrap(),
                reason: "fixture instrument reports an unsafe program effect".into(),
                idempotency_key: format!("signal-{key}"),
            },
            &AuthContext::principal(study.native.observer.principal_id.clone()),
        )
        .await
        .unwrap()
}

#[tokio::test]
#[cfg(feature = "experiments")]
async fn cas_conflict_refresh_and_read_only_receipt_recovery() {
    let study = prepare_study("cas").await;
    activate(&study).await;
    treatment(&study, false).await;
    let cutoff = &study.input.plan.execution.cutoff;
    let prepared = study
        .native
        .prepare_settlement(&study.input, cutoff)
        .await
        .unwrap();
    fixture::command(
        &study.nexus,
        &format!(
            "UPDATE {} SET ATTRIBUTES {{summary:\"concurrent bookkeeping\"}} EXPECT VERSION {}",
            literal(&prepared.skill_ref),
            prepared.expected_skill_version
        ),
    )
    .await;
    let error = study
        .native
        .execute_verdict_simulated(&prepared, cutoff)
        .await
        .unwrap_err();
    assert_eq!(error.code, KipErrorCode::VersionConflict);
    assert!(
        study
            .native
            .recover_verdict(&prepared)
            .await
            .unwrap()
            .is_none()
    );
    let refreshed = study
        .native
        .prepare_settlement(&study.input, cutoff)
        .await
        .unwrap();
    let receipt = study
        .native
        .execute_verdict_simulated(&refreshed, cutoff)
        .await
        .unwrap();
    assert_eq!(receipt.to_status, "adopted");
    let safety = signal(&study, "after-adoption").await;
    let withdrawal = study
        .native
        .prepare_safety_revocation(&safety.evidence_ref, "withdraw-after-adoption")
        .await
        .unwrap();
    study
        .native
        .execute_verdict(&withdrawal, &anda_cognitive_nexus::time::now())
        .await
        .unwrap();
    let sequence = study
        .nexus
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let recovered = study
        .native
        .recover_verdict(&refreshed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.evaluation_ref, receipt.evaluation_ref);
    assert!(recovered.native.response.is_none());
    assert_eq!(
        study
            .nexus
            .store
            .get_space(DEFAULT_SPACE)
            .await
            .unwrap()
            .seq,
        sequence
    );
    study.db.close().await.unwrap();
}

#[tokio::test]
#[cfg(feature = "experiments")]
async fn changed_cutoff_comparison_and_omitted_failure_cannot_commit() {
    let study = prepare_study("tamper").await;
    activate(&study).await;
    assert!(
        study
            .native
            .prepare_settlement(&study.input, &anda_cognitive_nexus::time::now())
            .await
            .is_err()
    );
    let (attempt, failed) = treatment(&study, true).await.unwrap();
    let cutoff = &study.input.plan.execution.cutoff;
    let prepared = study
        .native
        .prepare_settlement(&study.input, cutoff)
        .await
        .unwrap();
    assert_eq!(prepared.comparison["status"], "not_improved");
    let mut changed = prepared.clone();
    changed.cutoff = "2098-01-01T00:00:00.000Z".into();
    changed.evaluation["cutoff"] = json!(changed.cutoff);
    assert!(
        study
            .native
            .execute_verdict_simulated(&changed, cutoff)
            .await
            .is_err()
    );
    let mut changed = prepared.clone();
    changed.comparison["status"] = json!("improved");
    changed.to_status = "adopted".into();
    changed.evaluation["comparison"] = changed.comparison.clone();
    changed.evaluation["to_status"] = json!("adopted");
    assert!(
        study
            .native
            .execute_verdict_simulated(&changed, cutoff)
            .await
            .is_err()
    );
    let mut omitted = prepared.clone();
    omitted.evaluation["outcome_refs"]
        .as_array_mut()
        .unwrap()
        .retain(|v| v != &failed);
    omitted.replay["outcomes"]
        .as_object_mut()
        .unwrap()
        .remove(&failed);
    omitted.evaluation["missing_attempt_refs"]
        .as_array_mut()
        .unwrap()
        .push(json!(attempt));
    omitted.evaluation["replay_artifact"] =
        json!(super::super::super::execution::pin(&omitted.replay).unwrap());
    assert!(
        study
            .native
            .execute_verdict_simulated(&omitted, cutoff)
            .await
            .is_err()
    );
    assert!(
        study
            .native
            .recover_verdict(&omitted)
            .await
            .unwrap()
            .is_none()
    );
    study.db.close().await.unwrap();
}

#[tokio::test]
async fn revoked_standing_cannot_reenter_with_the_old_trial() {
    let study = prepare_study("reentry").await;
    activate(&study).await;
    let safety = signal(&study, "before-treatment").await;
    let withdrawal = study
        .native
        .prepare_safety_revocation(&safety.evidence_ref, "safety-withdraw")
        .await
        .unwrap();
    study
        .native
        .execute_verdict(&withdrawal, &anda_cognitive_nexus::time::now())
        .await
        .unwrap();
    let mut input = study.input.clone();
    input.idempotency_key = "reuse-old-trial".into();
    let now = anda_cognitive_nexus::time::now();
    let prepared = study
        .native
        .prepare_activation(&input, &now)
        .await
        .unwrap()
        .unwrap();
    let error = study
        .native
        .execute_verdict(&prepared, &now)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("trial opened after revocation"),
        "{error:?}"
    );
    study.db.close().await.unwrap();
}

#[tokio::test]
#[cfg(feature = "experiments")]
async fn independent_safety_signal_stops_a_different_active_monitoring_trial() {
    let study = prepare_study("monitor").await;
    activate(&study).await;
    treatment(&study, false).await;
    let prepared = study
        .native
        .prepare_settlement(&study.input, &study.input.plan.execution.cutoff)
        .await
        .unwrap();
    let adopted = study
        .native
        .execute_verdict_simulated(&prepared, &study.input.plan.execution.cutoff)
        .await
        .unwrap();
    let mut plan = study.input.plan.clone();
    plan.execution.review_of = Some(super::super::super::AdoptionBasis {
        revision_ref: plan.candidate_revision.clone(),
        trial_ref: study.input.trial_ref.clone(),
        evaluation_ref: adopted.evaluation_ref.clone(),
    });
    let frozen = install(&study.native, &plan, "new-monitor").await;
    let trial_ref = baseline(&study.native, &plan, &frozen, &study.basis, "new-monitor").await;
    let input = NativeVerdictInput {
        plan,
        frozen,
        trial_ref,
        idempotency_key: "monitor-activate".into(),
    };
    assert!(
        study
            .native
            .prepare_activation(&input, &anda_cognitive_nexus::time::now())
            .await
            .unwrap()
            .is_none()
    );
    study.native.validate_active_trial(&input).await.unwrap();
    let denied = NativeSafetySignalInput {
        revision_ref: study.input.plan.candidate_revision.clone(),
        observation_key: "forged".into(),
        observed_at: anda_cognitive_nexus::time::now(),
        journal_digest: content_digest(&json!("forged")).unwrap(),
        reason: "forged".into(),
        idempotency_key: "forged-safety".into(),
    };
    assert!(
        study
            .native
            .record_safety_signal(&denied, &AuthContext::system())
            .await
            .is_err()
    );
    let safety = signal(&study, "independent-monitor-alert").await;
    let withdrawal = study
        .native
        .prepare_safety_revocation(&safety.evidence_ref, "monitor-withdraw")
        .await
        .unwrap();
    study
        .native
        .execute_verdict(&withdrawal, &anda_cognitive_nexus::time::now())
        .await
        .unwrap();
    assert!(study.native.validate_active_trial(&input).await.is_err());
    let original = study
        .native
        .read_record(
            &study.native.writer(None).unwrap(),
            &adopted.evaluation_ref,
            "EvaluationRecord",
        )
        .await
        .unwrap();
    assert_eq!(original["record"]["to_status"], "adopted");
    study.db.close().await.unwrap();
}

#[tokio::test]
#[cfg(feature = "experiments")]
async fn correcting_adoption_evidence_invalidates_the_current_evidence_gate() {
    let study = prepare_study("corrected-proof").await;
    activate(&study).await;
    treatment(&study, false).await;
    let prepared = study
        .native
        .prepare_settlement(&study.input, &study.input.plan.execution.cutoff)
        .await
        .unwrap();
    let receipt = study
        .native
        .execute_verdict_simulated(&prepared, &study.input.plan.execution.cutoff)
        .await
        .unwrap();
    study
        .native
        .validate_current_verdict_evidence(&study.input, &receipt.evaluation_ref)
        .await
        .unwrap();
    let old = prepared.evaluation["outcome_refs"][0].as_str().unwrap();
    let corrected = fixture::command(&study.nexus, r#"CREATE EVIDENCE ?correction {SET FIELDS {evidence_class:"observation",payload:"independent audit corrects the prior result"}}"#).await;
    fixture::command(
        &study.nexus,
        &format!(
            "TRANSITION {} TO \"corrected\" BY {}",
            literal(old),
            literal(corrected["handles"]["correction"].as_str().unwrap())
        ),
    )
    .await;
    let sequence = study
        .nexus
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    assert!(
        study
            .native
            .validate_current_verdict_evidence(&study.input, &receipt.evaluation_ref)
            .await
            .is_err()
    );
    assert_eq!(
        study
            .nexus
            .store
            .get_space(DEFAULT_SPACE)
            .await
            .unwrap()
            .seq,
        sequence
    );
    let skill = fixture::command(
        &study.nexus,
        &format!(
            "FIND(?s) WHERE {{?s CONCEPT {{id:{}}}}}",
            literal(&prepared.skill_ref)
        ),
    )
    .await;
    assert_eq!(
        skill[0]["attributes"]["status"], "adopted",
        "read gate does not rewrite historical standing"
    );
    study.db.close().await.unwrap();
}
