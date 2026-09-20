use super::*;
use crate::consequence::*;

#[tokio::test]
async fn r6_paired_utility_reuses_native_verdict_and_cannot_make_revoked_skill_executable() {
    let store = Arc::new(object_store::memory::InMemory::new());
    let (space, rt, clock, cfg, plan, basis) = p4::fixture(store.clone()).await;
    rt.enroll("r6-paired".into(), plan.clone(), basis)
        .await
        .unwrap();
    p4::cohort(
        &rt,
        "r6-paired",
        Executor::new(&cfg, &plan, space.memory.nexus()),
        false,
        true,
    )
    .await;
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let settled = rt.settle("r6-paired".into()).await.unwrap();
    let mut config = UtilityConfig {
        version: "paired-utility-fixture-v1".into(),
        method: AttributionMethod::PairedRevisionV1,
        observer: cfg.observer.clone(),
        task_family: plan.task_family.clone(),
        metric: "success".into(),
        window: plan.observation_window.clone(),
        environment_digest: plan.environment_digest.clone(),
        tool_versions: plan.tool_versions.clone(),
        parameters: Some(UtilityParameters {
            step_cap: 0.1,
            minimum_independent_samples: 2,
            gain: 0.5,
            minimum_confidence: 0.9,
            initial_utility: Some(0.5),
        }),
        calibration: None,
        automatic: false,
        apply: true,
        rank: true,
    };
    config.calibration = Some(UtilityCalibration {
        reviewed_by: "kip:principal:fixture-reviewer".into(),
        contract_digest: config.contract_digest().unwrap(),
        approved: true,
        material: json!({"protocol_test_only":true}),
    });
    let utility = crate::consequence::utility::UtilityRuntime::new(
        space.memory.nexus(),
        store,
        space.recall_receipts(),
        Some(config.clone()),
        true,
        Some(Arc::downgrade(&rt)),
    );
    utility
        .enqueue(settled.evaluation_ref.clone().unwrap())
        .await
        .unwrap();
    let applied = utility
        .evaluate(plan.candidate_revision.clone())
        .await
        .unwrap();
    assert_eq!(applied.receipt.status, "applied", "{applied:?}");
    assert_eq!(applied.receipt.independent_samples, 8);
    assert_eq!(applied.receipt.new_value, Some(0.6));
    let revision = crate::runtime_api::full_read(
        &space.memory.nexus().system_session(),
        &plan.candidate_revision,
    )
    .await
    .unwrap();
    let ranked = utility
        .rank(&[crate::recall_receipt::pin(&revision).unwrap()])
        .await
        .unwrap();
    assert_eq!(
        ranked.get(&plan.candidate_revision),
        Some(&0.6),
        "revision={}, sample={:?}",
        revision["_system"],
        rt.utility_sample(settled.evaluation_ref.as_deref().unwrap(), &config)
            .await
    );
    let skill = p4::skill(&space.memory.nexus()).await;
    rt.submit_safety_signal(
        auth(),
        SafetySubmission {
            space_instance: settled.space_instance,
            job_id: "r6-paired".into(),
            signal_key: "r6-safety".into(),
            observed_at: anda_cognitive_nexus::time::now(),
            evidence_digest: content_digest(&json!("independent safety fixture")).unwrap(),
            reason: "unsafe action".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        !rt.procedure_status(&skill)
            .await
            .unwrap()
            .recommendation_allowed
    );
    assert!(
        utility
            .rank(&[crate::recall_receipt::pin(&revision).unwrap()])
            .await
            .unwrap()
            .is_empty()
    );
    utility.shutdown().await;
    rt.shutdown().await;
    space.close().await.unwrap();
}
