use super::*;
use crate::{consequence::utility::*, consequence::*, recall_receipt::RecallReceiptRef};
const OBSERVER: &str = "kip:principal:r6-observer";

fn config() -> UtilityConfig {
    let mut config = UtilityConfig {
        version: "single-contribution/test-v1".into(),
        method: AttributionMethod::SingleContributionV1,
        observer: anda_kip::cognitive::ObserverControl {
            principal_id: OBSERVER.into(),
            configuration_digest: pin("r6-instrument").digest,
            control_domain: "independent-instrument".into(),
        },
        task_family: "memory.reminder".into(),
        metric: "completed".into(),
        window: "actual-task-v1".into(),
        environment_digest: pin("r3-env").digest,
        tool_versions: BTreeMap::from([("fixture".into(), "v1".into())]),
        parameters: Some(UtilityParameters {
            step_cap: 0.1,
            minimum_independent_samples: 1,
            gain: 0.5,
            minimum_confidence: 0.9,
            initial_utility: Some(0.5),
        }),
        calibration: None,
        automatic: true,
        apply: true,
        rank: true,
    };
    config.calibration = Some(UtilityCalibration {
        reviewed_by: "kip:principal:fixture-reviewer".into(),
        contract_digest: config.contract_digest().unwrap(),
        approved: true,
        material: json!({"mechanism_fixture":true,"not_empirical_calibration":true}),
    });
    config
}
struct Fixture {
    app: AppState,
    space: Arc<Space>,
    runtime: Arc<UtilityRuntime>,
    consequences: Arc<ConsequenceRuntime>,
    config: UtilityConfig,
    target: String,
    other: String,
    gate: ActionStatus,
    receipt: RecallReceiptRef,
    native_instance: String,
}
async fn fixture(name: &str, used: usize) -> Fixture {
    fixture_store(name, used, None).await
}
async fn fixture_store(
    name: &str,
    used: usize,
    store: Option<Arc<dyn object_store::ObjectStore>>,
) -> Fixture {
    let p = policy(act());
    let e = Arc::new(Executor::default());
    let (app, space, _, _) = setup_store(name, bindings(p.clone(), e), &p, store).await;
    principal(
        &space,
        OBSERVER,
        &["read", "read_history", "create", "derive", "record_outcome"],
    )
    .await;
    let target=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Preference" NAME "verified useful memory" SET FACET "MnemonicState" {utility:0.5,memory_strength:0.7}}"#,Default::default()).await;
    let other=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Preference" NAME "retrieved only" SET FACET "MnemonicState" {utility:0.2,memory_strength:0.6}}"#,Default::default()).await;
    p.context.lock().unwrap().as_mut().unwrap().required_refs = vec![target.clone(), other.clone()];
    *p.proposal.lock().unwrap() = Proposal::Act {
        rationale: "Use the selected memory in this actual host operation".into(),
        used_refs: vec![target.clone(), other.clone()]
            .into_iter()
            .take(used)
            .collect(),
        payload: json!({"text":"registered operation"}),
    };
    let watch=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"R6 contribution",status:"disarmed",condition:{element: :target}}}"#,kip::param("target",target.clone())).await;
    space.attention().arm_watch(watch.clone(), 1).await.unwrap();
    seed_kip(
        &space,
        kip::request_with(
            r#"UPDATE :target SET FIELDS {name:"actual delivered memory"}"#,
            kip::param("target", target.clone()),
        ),
    )
    .await;
    let fire = space
        .memory
        .nexus()
        .session(auth(HOST))
        .advance_watch(DEFAULT_SPACE, &watch, 2, 1, 200)
        .await
        .unwrap();
    let wake = fire["wake_ref"].as_str().unwrap().to_string();
    let gate = process(&space, &wake).await;
    assert_eq!(gate.decision.as_deref(), Some("act"), "{gate:?}");
    let child = dispatch_child(&space, &wake).await;
    let dispatched = process(&space, &child.wake_ref).await;
    assert!(
        dispatched
            .reason
            .as_deref()
            .is_none_or(|r| !r.contains("failed")),
        "{dispatched:?}"
    );
    let d = crate::runtime_api::full_read(
        &space.memory.nexus().system_session(),
        gate.decision_ref.as_deref().unwrap(),
    )
    .await
    .unwrap();
    let rationale: Json = serde_json::from_str(
        d["facets"][format!("{PROFILE}DecisionRecord")]["rationale"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let receipt = serde_json::from_value(rationale["recall_receipt"].clone()).unwrap();
    let config = config();
    let scope = child.scope.clone();
    let native_instance = scope.space_instance.clone();
    let consequences = ConsequenceRuntime::new(
        space.memory.nexus(),
        app.attention_directory.clone(),
        scope,
        vec![ObserverContract {
            principal_id: OBSERVER.into(),
            configuration_digest: config.observer.configuration_digest.clone(),
            control_domain: config.observer.control_domain.clone(),
            task_family: config.task_family.clone(),
            metric: config.metric.clone(),
            window: config.window.clone(),
            maximum_delay_ms: 60_000,
        }],
        false,
        #[cfg(feature = "learning")]
        Some(Arc::downgrade(&space.learning())),
    );
    let runtime = UtilityRuntime::new(
        space.memory.nexus(),
        app.object_store.clone(),
        space.recall_receipts(),
        Some(config.clone()),
        true,
        #[cfg(feature = "learning")]
        Some(Arc::downgrade(&space.learning())),
    );
    runtime.bind_consequences(Arc::downgrade(&consequences));
    Fixture {
        app,
        space,
        runtime,
        consequences,
        config,
        target,
        other,
        gate,
        receipt,
        native_instance,
    }
}
async fn outcome(f: &Fixture, event: &str, unit: &str, writer: &str) -> ObservationReceipt {
    let delivered = f.space.recall_receipts().read(&f.receipt).await.unwrap();
    let witness = ContributionWitness {
        format: "anda-brain:single-contribution-v1".into(),
        contract_digest: f.config.contract_digest().unwrap(),
        sampling_unit: unit.into(),
        attempt_ref: f.gate.attempt_ref.clone().unwrap(),
        decision_ref: f.gate.decision_ref.clone().unwrap(),
        recall_receipt: f.receipt.clone(),
        target: delivered
            .pins
            .iter()
            .find(|p| p.id == f.target)
            .unwrap()
            .clone(),
        effect: 0.6,
        lower_bound: 0.4,
        upper_bound: 0.8,
        confidence: 0.99,
        isolated_contribution: true,
    };
    let command = format!(
        r#"CREATE EVIDENCE ?w {{SET FIELDS {{evidence_class:"artifact",payload:{},observed_at:{}}}}}"#,
        kip::string_literal(&serde_json::to_string(&witness).unwrap()),
        kip::string_literal(&anda_cognitive_nexus::time::now())
    );
    let session = f.space.memory.nexus().session(auth(writer));
    let r = anda_kip::execute_request(&session, &kip::request(command)).await;
    let witness_ref = kip::ok_result(&r).unwrap_or_else(|| panic!("{r:?}"))["handles"]["w"]
        .as_str()
        .unwrap()
        .to_string();
    f.consequences
        .submit(
            auth(OBSERVER),
            OutcomeInput {
                space_instance: f.native_instance.clone(),
                attempt_ref: witness.attempt_ref,
                observer_configuration_digest: f.config.observer.configuration_digest.clone(),
                event_key: event.into(),
                observed_at: anda_cognitive_nexus::time::now(),
                metric: f.config.metric.clone(),
                window: f.config.window.clone(),
                observation: Observation::Measurement {
                    terminal: true,
                    outcome_status: OutcomeStatus::Success,
                    magnitude: Some(1.0),
                    payload: json!({"actual_instrument_fixture":true}),
                },
                correction_of: None,
                safety_signal: None,
                utility: Some(UtilityAttribution {
                    witness_ref: Some(witness_ref),
                    witness: None,
                }),
            },
        )
        .await
        .unwrap()
}
async fn value(f: &Fixture, id: &str) -> Json {
    crate::runtime_api::full_read(&f.space.memory.nexus().system_session(), id)
        .await
        .unwrap()
}

#[tokio::test]
async fn r6_retrieval_counts_and_missing_parameters_never_write_utility() {
    let mut f = fixture("r6_no_params", 1).await;
    let before = value(&f, &f.target).await;
    for _ in 0..4 {
        f.space
            .ledger
            .record_recall(&[f.target.clone()].into(), unix_ms())
            .await
            .unwrap();
    }
    assert_eq!(value(&f, &f.target).await["facets"], before["facets"]);
    f.config.parameters = None;
    f.config.calibration = None;
    f.config.apply = false;
    f.runtime = UtilityRuntime::new(
        f.space.memory.nexus(),
        f.app.object_store.clone(),
        f.space.recall_receipts(),
        Some(f.config.clone()),
        true,
        #[cfg(feature = "learning")]
        Some(Arc::downgrade(&f.space.learning())),
    );
    f.runtime.bind_consequences(Arc::downgrade(&f.consequences));
    let receipt = outcome(&f, "event", "unit", OBSERVER).await;
    f.runtime
        .enqueue(receipt.outcome_ref.unwrap())
        .await
        .unwrap();
    let result = f.runtime.evaluate(f.target.clone()).await.unwrap();
    assert_eq!(result.receipt.reason.as_deref(), Some("parameters_missing"));
    assert_eq!(result.receipt.new_value, Some(0.5));
    f.runtime.shutdown().await;
    f.consequences.shutdown().await;
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r6_inline_signed_witness_supports_negative_effect_without_extra_native_evidence() {
    let f = fixture("r6_inline", 1).await;
    let delivered = f.space.recall_receipts().read(&f.receipt).await.unwrap();
    let witness = ContributionWitness {
        format: "anda-brain:single-contribution-v1".into(),
        contract_digest: f.config.contract_digest().unwrap(),
        sampling_unit: "inline-unit".into(),
        attempt_ref: f.gate.attempt_ref.clone().unwrap(),
        decision_ref: f.gate.decision_ref.clone().unwrap(),
        recall_receipt: f.receipt.clone(),
        target: delivered
            .pins
            .into_iter()
            .find(|p| p.id == f.target)
            .unwrap(),
        effect: -0.6,
        lower_bound: -0.8,
        upper_bound: -0.4,
        confidence: 0.99,
        isolated_contribution: true,
    };
    let receipt = f
        .consequences
        .submit(
            auth(OBSERVER),
            OutcomeInput {
                space_instance: f.native_instance.clone(),
                attempt_ref: witness.attempt_ref.clone(),
                observer_configuration_digest: f.config.observer.configuration_digest.clone(),
                event_key: "negative-event".into(),
                observed_at: anda_cognitive_nexus::time::now(),
                metric: f.config.metric.clone(),
                window: f.config.window.clone(),
                observation: Observation::Measurement {
                    terminal: true,
                    outcome_status: OutcomeStatus::Failure,
                    magnitude: Some(0.0),
                    payload: json!({"actual_effect":"adverse"}),
                },
                correction_of: None,
                safety_signal: None,
                utility: Some(UtilityAttribution {
                    witness: Some(witness),
                    witness_ref: None,
                }),
            },
        )
        .await
        .unwrap();
    f.runtime
        .enqueue(receipt.outcome_ref.unwrap())
        .await
        .unwrap();
    let result = f.runtime.evaluate(f.target.clone()).await.unwrap();
    assert_eq!(result.receipt.status, "applied", "{result:?}");
    assert_eq!(result.receipt.new_value, Some(0.4));
    assert_eq!(result.receipt.initial_assumption, Some(0.5));
    f.runtime.shutdown().await;
    f.consequences.shutdown().await;
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r6_correction_suspends_ranking_and_records_new_no_update_without_rewriting_history() {
    let f = fixture("r6_correction", 1).await;
    let receipt = outcome(&f, "original", "unit", OBSERVER).await;
    f.runtime
        .enqueue(receipt.outcome_ref.clone().unwrap())
        .await
        .unwrap();
    let applied = f.runtime.evaluate(f.target.clone()).await.unwrap();
    let original = value(&f, applied.evidence_ref.as_deref().unwrap()).await;
    let correction = f
        .consequences
        .submit(
            auth(OBSERVER),
            OutcomeInput {
                space_instance: f.native_instance.clone(),
                attempt_ref: f.gate.attempt_ref.clone().unwrap(),
                observer_configuration_digest: f.config.observer.configuration_digest.clone(),
                event_key: "correction".into(),
                observed_at: anda_cognitive_nexus::time::now(),
                metric: f.config.metric.clone(),
                window: f.config.window.clone(),
                observation: Observation::Measurement {
                    terminal: true,
                    outcome_status: OutcomeStatus::Failure,
                    magnitude: Some(0.0),
                    payload: json!({"correction":"instrument rejected original effect"}),
                },
                correction_of: Some("original".into()),
                safety_signal: None,
                utility: None,
            },
        )
        .await
        .unwrap();
    let p = crate::recall_receipt::pin(&value(&f, &f.target).await).unwrap();
    assert!(f.runtime.rank(&[p]).await.unwrap().is_empty());
    f.runtime
        .enqueue(correction.outcome_ref.unwrap())
        .await
        .unwrap();
    let revised = f.runtime.evaluate(f.target.clone()).await.unwrap();
    assert_eq!(revised.receipt.status, "no_update");
    assert_eq!(revised.receipt.new_value, Some(0.6));
    assert_ne!(revised.evidence_ref, applied.evidence_ref);
    assert_eq!(revised.receipt.previous_receipt, applied.evidence_ref);
    assert_eq!(
        value(&f, applied.evidence_ref.as_deref().unwrap()).await["payload"],
        original["payload"]
    );
    f.runtime.shutdown().await;
    f.consequences.shutdown().await;
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r6_native_commit_checkpoint_loss_cold_replays_once() {
    use anda_object_store::fault::{FaultOp, FaultRule, FaultStore};
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let store: Arc<dyn object_store::ObjectStore> = Arc::new(store);
    let f = fixture_store("r6_recovery", 1, Some(store.clone())).await;
    let receipt = outcome(&f, "event", "unit", OBSERVER).await;
    f.runtime
        .enqueue(receipt.outcome_ref.unwrap())
        .await
        .unwrap();
    faults.push_rule(FaultRule {
        skip: 1,
        ..FaultRule::fail_once(FaultOp::Put, "/utility-targets/")
    });
    assert!(f.runtime.evaluate(f.target.clone()).await.is_err());
    assert_eq!(
        value(&f, &f.target).await["facets"][format!("{PROFILE}MnemonicState")]["utility"],
        0.6
    );
    faults.reset();
    f.runtime.shutdown().await;
    f.consequences.shutdown().await;
    f.space.close().await.unwrap();
    let mut app = test_app_state("r6_recovery").fork_with_store(store.clone());
    app.automatic = true;
    let space = app
        .load_space_with("r6_recovery", false, false)
        .await
        .unwrap();
    let runtime = UtilityRuntime::new(
        space.memory.nexus(),
        store,
        space.recall_receipts(),
        Some(f.config),
        true,
        #[cfg(feature = "learning")]
        Some(Arc::downgrade(&space.learning())),
    );
    let seq = space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let recovered = runtime.evaluate(f.target.clone()).await.unwrap();
    assert_eq!(recovered.receipt.status, "applied");
    assert_eq!(recovered.receipt.new_value, Some(0.6));
    assert_eq!(
        space
            .memory
            .nexus()
            .store
            .get_space(DEFAULT_SPACE)
            .await
            .unwrap()
            .seq,
        seq
    );
    runtime.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn r6_changed_target_version_recomputes_and_cancelled_waiter_keeps_owned_commit() {
    use anda_object_store::fault::{FaultGate, FaultKind, FaultOp, FaultRule, FaultStore};
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let f = fixture_store("r6_cas", 1, Some(Arc::new(store))).await;
    let receipt = outcome(&f, "event", "unit", OBSERVER).await;
    f.runtime
        .enqueue(receipt.outcome_ref.unwrap())
        .await
        .unwrap();
    let gate = FaultGate::new();
    faults.push_rule(FaultRule {
        kind: FaultKind::PauseAfter(gate.clone()),
        ..FaultRule::fail_once(FaultOp::Put, "/utility-targets/")
    });
    let running = {
        let rt = f.runtime.clone();
        let target = f.target.clone();
        tokio::spawn(async move { rt.evaluate(target).await })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_entered())
        .await
        .unwrap();
    seed_kip(
        &f.space,
        kip::request_with(
            r#"UPDATE :target SET FACET "MnemonicState" {memory_strength:0.71}"#,
            kip::param("target", f.target.clone()),
        ),
    )
    .await;
    gate.release();
    assert!(running.await.unwrap().is_err());
    faults.reset();
    assert_eq!(
        value(&f, &f.target).await["facets"][format!("{PROFILE}MnemonicState")]["utility"],
        0.5
    );
    let hold = FaultGate::new();
    faults.push_rule(FaultRule {
        kind: FaultKind::PauseAfter(hold.clone()),
        ..FaultRule::fail_once(FaultOp::Put, "/evidence/")
    });
    let waiter = {
        let rt = f.runtime.clone();
        let target = f.target.clone();
        tokio::spawn(async move { rt.evaluate(target).await })
    };
    tokio::time::timeout(Duration::from_secs(5), hold.wait_entered())
        .await
        .unwrap();
    waiter.abort();
    let _ = waiter.await;
    let closing = {
        let rt = f.runtime.clone();
        tokio::spawn(async move { rt.shutdown().await })
    };
    sleep(Duration::from_millis(20)).await;
    assert!(!closing.is_finished());
    hold.release();
    closing.await.unwrap();
    faults.reset();
    let row = value(&f, &f.target).await;
    assert_eq!(
        row["facets"][format!("{PROFILE}MnemonicState")]["utility"],
        0.6
    );
    assert_eq!(
        row["facets"][format!("{PROFILE}MnemonicState")]["memory_strength"],
        0.71
    );
    f.consequences.shutdown().await;
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r6_independent_single_contribution_updates_atomically_and_never_rewards_retrieved_only() {
    let f = fixture("r6_single", 1).await;
    let before = value(&f, &f.other).await;
    let receipt = outcome(&f, "event", "actual-unit-1", OBSERVER).await;
    f.runtime
        .enqueue(receipt.outcome_ref.clone().unwrap())
        .await
        .unwrap();
    let result = f.runtime.evaluate(f.target.clone()).await.unwrap();
    assert_eq!(result.receipt.status, "applied", "{result:?}");
    assert_eq!(result.receipt.new_value, Some(0.6));
    let after = value(&f, &f.target).await;
    assert_eq!(
        after["facets"][format!("{PROFILE}MnemonicState")]["utility"],
        0.6
    );
    assert_eq!(
        after["facets"][format!("{PROFILE}MnemonicState")]["memory_strength"],
        0.7
    );
    assert_eq!(value(&f, &f.other).await, before);
    let native = value(&f, result.evidence_ref.as_deref().unwrap()).await;
    assert_eq!(
        native["_system"]["created_tx"],
        after["_system"]["updated_tx"]
    );
    let seq = f
        .space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let duplicate = f.runtime.evaluate(f.target.clone()).await.unwrap();
    assert_eq!(duplicate.evidence_ref, result.evidence_ref);
    assert_eq!(
        f.space
            .memory
            .nexus()
            .store
            .get_space(DEFAULT_SPACE)
            .await
            .unwrap()
            .seq,
        seq
    );
    assert_eq!(
        f.runtime
            .rank(&[crate::recall_receipt::pin(&after).unwrap()])
            .await
            .unwrap()[&f.target],
        0.6
    );
    f.runtime.shutdown().await;
    f.consequences.shutdown().await;
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r6_bundle_and_unqualified_witness_never_create_individual_credit() {
    for (name, n, writer, reason) in [
        ("r6_bundle", 2, OBSERVER, "inseparable_bundle"),
        ("r6_forged", 1, HOST, "contribution_witness_not_qualified"),
    ] {
        let f = fixture(name, n).await;
        let receipt = outcome(&f, "event", "unit-1", writer).await;
        f.runtime
            .enqueue(receipt.outcome_ref.unwrap())
            .await
            .unwrap();
        let result = f.runtime.evaluate(f.target.clone()).await.unwrap();
        assert_eq!(result.receipt.status, "no_update", "{result:?}");
        assert!(result.receipt.excluded.values().any(|r| r == reason));
        assert_eq!(
            value(&f, &f.target).await["facets"][format!("{PROFILE}MnemonicState")]["utility"],
            0.5
        );
        f.runtime.shutdown().await;
        f.consequences.shutdown().await;
        f.space.close().await.unwrap();
    }
}
