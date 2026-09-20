use super::*;
use crate::consequence::{ConsequenceRuntime, Observation, ObservationLane, OutcomeInput};

pub(super) fn input(ticket: &DispatchTicket, submission: &OutcomeSubmission) -> OutcomeInput {
    OutcomeInput {
        utility: None,
        space_instance: submission.space_instance.clone(),
        attempt_ref: ticket.attempt_ref.clone(),
        observer_configuration_digest: submission.observer_configuration_digest.clone(),
        event_key: submission.observation_key.clone(),
        observed_at: submission.observed_at.clone(),
        metric: "success".into(),
        window: ticket.plan.observation_window.clone(),
        observation: Observation::Learning {
            measurements: json!(submission.measurements),
        },
        correction_of: None,
        safety_signal: None,
    }
}
pub(super) async fn ingress(
    space: &crate::space::Space,
    store: Arc<dyn ObjectStore>,
    runtime: &Arc<LearningRuntime>,
) -> Arc<ConsequenceRuntime> {
    space.attention().register_work().await.unwrap();
    let native = space
        .memory
        .nexus()
        .system_session()
        .read_control(DEFAULT_SPACE, "attention/config", None)
        .await
        .unwrap()
        .unwrap();
    let cfg: anda_cognitive_nexus::attention::AttentionConfig =
        serde_json::from_value(native.value).unwrap();
    ConsequenceRuntime::new(
        space.memory.nexus(),
        crate::attention::Directory::new(store, 0),
        cfg.scope,
        vec![],
        false,
        Some(Arc::downgrade(runtime)),
    )
}

#[tokio::test]
async fn r4_registered_baseline_and_treatment_keep_one_native_outcome_writer() {
    let (store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("r4-job".into(), plan.clone(), basis)
        .await
        .unwrap();
    let ingress = ingress(&space, store, &rt).await;
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    for n in 0..4 {
        let DriveResult::Dispatched(ticket) =
            rt.drive("r4-job".into(), executor.clone()).await.unwrap()
        else {
            panic!("dispatch missing")
        };
        if n < 2 {
            assert!(matches!(ticket.arm, NativeArm::Baseline));
        }
        let observation = outcome(&ticket, n >= 2);
        let request = input(&ticket, &observation);
        let first = ingress.submit(auth(), request.clone()).await.unwrap();
        assert!(first.native_committed && first.learning_eligible);
        let second = ingress.submit(auth(), request).await.unwrap();
        assert_eq!(first.outcome_ref, second.outcome_ref);
        let job = rt.report("r4-job").await.unwrap();
        assert_eq!(job.outcome_cursor, n + 1);
    }
    let evidence = command(
        &space.memory.nexus(),
        r#"FIND(?e.id) WHERE {?e EVIDENCE {evidence_class:"outcome"}} LIMIT 10"#,
    )
    .await;
    assert_eq!(evidence.as_array().unwrap().len(), 4);
    ingress.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_nonterminal_and_late_learning_material_stay_audit_only_with_safety_visible() {
    let (store, _app, space, cfg, mut plan, basis) = setup().await;
    let now = anda_engine::unix_ms();
    plan.execution.cutoff = crate::kip::timestamp(now + 60_000);
    plan.execution.review_due_at = crate::kip::timestamp(now + 120_000);
    let clock = crate::runtime::BusinessClock::manual(now).unwrap();
    let rt = LearningRuntime::connect(
        store.clone(),
        "p3_space/learning".into(),
        space.memory.nexus(),
        clock.clone(),
    )
    .await
    .unwrap();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("r4-late".into(), plan.clone(), basis)
        .await
        .unwrap();
    let ingress = ingress(&space, store, &rt).await;
    let DriveResult::Dispatched(ticket) = rt
        .drive(
            "r4-late".into(),
            Executor::new(&cfg, &plan, space.memory.nexus()),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let mut progress = outcome(&ticket, false);
    progress.measurements.finished = false;
    let receipt = ingress
        .submit(auth(), input(&ticket, &progress))
        .await
        .unwrap();
    assert_eq!(receipt.status, "progress_audit");
    assert!(!receipt.native_committed);
    clock.advance_to(now + 61_000).unwrap();
    let mut late = outcome(&ticket, false);
    late.observation_key = "late-measurement".into();
    let mut request = input(&ticket, &late);
    request.safety_signal = Some("severe independent safety event".into());
    let receipt = ingress.submit(auth(), request).await.unwrap();
    assert_eq!(receipt.status, "late_audit");
    assert!(!receipt.learning_eligible && receipt.safety_pending);
    let audit = ingress
        .receipts(auth(), ObservationLane::Learning, 0, 100)
        .await
        .unwrap();
    assert!(audit.items.iter().any(|r| r.safety_pending));
    assert_eq!(rt.report("r4-late").await.unwrap().outcome_cursor, 0);
    ingress.shutdown().await;
    rt.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_ingress_recovers_a_native_learning_ack_loss_after_cutoff() {
    let fault = Arc::new(fault_store::CheckpointFault::default());
    let (store, _app, space, cfg, mut plan, basis) = setup_store(fault.clone()).await;
    let now = anda_engine::unix_ms();
    plan.execution.cutoff = crate::kip::timestamp(now + 60_000);
    plan.execution.review_due_at = crate::kip::timestamp(now + 120_000);
    let clock = crate::runtime::BusinessClock::manual(now).unwrap();
    let rt = LearningRuntime::connect(
        store.clone(),
        "p3_space/learning".into(),
        space.memory.nexus(),
        clock.clone(),
    )
    .await
    .unwrap();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("r4-recovery".into(), plan.clone(), basis)
        .await
        .unwrap();
    let ingress = ingress(&space, store, &rt).await;
    let DriveResult::Dispatched(ticket) = rt
        .drive(
            "r4-recovery".into(),
            Executor::new(&cfg, &plan, space.memory.nexus()),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let observation = outcome(&ticket, false);
    fault.fail_checkpoint.store(true, Ordering::SeqCst);
    assert!(
        rt.submit_outcome(auth(), observation.clone())
            .await
            .is_err()
    );
    assert_eq!(rt.report("r4-recovery").await.unwrap().outcome_cursor, 0);
    clock.advance_to(now + 61_000).unwrap();
    let receipt = ingress
        .submit(auth(), input(&ticket, &observation))
        .await
        .unwrap();
    assert!(receipt.native_committed && receipt.learning_eligible);
    assert_eq!(rt.report("r4-recovery").await.unwrap().outcome_cursor, 1);
    let evidence = command(
        &space.memory.nexus(),
        r#"FIND(?e.id) WHERE {?e EVIDENCE {evidence_class:"outcome"}} LIMIT 10"#,
    )
    .await;
    assert_eq!(evidence.as_array().unwrap().len(), 1);
    ingress.shutdown().await;
    rt.shutdown().await;
    space.close().await.unwrap();
}
