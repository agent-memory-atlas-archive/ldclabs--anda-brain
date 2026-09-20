use super::*;

async fn faulty(name: &str) -> (Fixture, FaultHandle) {
    let (store, faults) = FaultStore::wrap(InMemory::new());
    (
        fixture_with(name, true, Arc::new(store), None).await,
        faults,
    )
}
pub(super) async fn reopen(f: Fixture) -> Fixture {
    let Fixture {
        app,
        space,
        runtime,
        cfg,
        actor,
        context,
        other_context,
        claims,
        propositions,
        governor_grant,
    } = f;
    let name = space.id.clone();
    space.close().await.unwrap();
    drop(runtime);
    drop(space);
    evict(&app).await;
    let mut reopened = test_app_state(&name).fork_with_store(app.object_store());
    reopened.automatic = true;
    let app = reopened
        .with_memory_runtime_bindings(bindings_for(&name, cfg.clone()))
        .unwrap();
    let space = app.load_space_with(&name, false, false).await.unwrap();
    let runtime = space.trust();
    Fixture {
        app,
        space,
        runtime,
        cfg,
        actor,
        context,
        other_context,
        claims,
        propositions,
        governor_grant,
    }
}

#[tokio::test]
async fn r8_native_commit_and_host_checkpoint_loss_recover_one_control_and_audit() {
    let (f, faults) = faulty("r8_recovery").await;
    record(&f, 0, "fact-a", "root-a", false).await;
    record(&f, 1, "fact-b", "root-b", false).await;
    faults.push_rule(FaultRule::fail_once(FaultOp::Put, "/targets/"));
    assert!(f.runtime.propose(f.actor.clone()).await.is_err());
    faults.reset();
    let p = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert_eq!(f.runtime.proposals(0, 32).await.unwrap().0[0].id, p.id);

    // First proposal PUT saves the reviewed intent. The next stores the result
    // after the native control/audit transaction has already committed.
    faults.push_rule(FaultRule {
        skip: 1,
        ..FaultRule::fail_once(FaultOp::Put, "/proposals/")
    });
    assert!(
        f.runtime
            .apply(
                auth(GOVERNOR),
                p.id.clone(),
                "approved before ACK loss".into()
            )
            .await
            .is_err()
    );
    assert_eq!(trust(&f).await.version, 2);
    assert_eq!(audits(&f).await, 1);
    faults.reset();
    let replacement=created_ref(&f.space,r#"CREATE EVIDENCE ?item {SET FIELDS {evidence_class:"observation",payload:"later correction"}}"#,Default::default()).await;
    seed_kip(
        &f.space,
        kip::request_with(
            "TRANSITION :id TO \"corrected\" BY :replacement",
            serde_json::from_value(json!({"id":p.evidence_refs[0],"replacement":replacement}))
                .unwrap(),
        ),
    )
    .await;
    let saved = trust(&f).await;
    let mut current = TrustConfiguration {
        weights: serde_json::from_value(saved.value["weights"].clone()).unwrap(),
        default_weight: saved.value["default_weight"].as_f64().unwrap(),
        rules: serde_json::from_value(saved.value["rules"].clone()).unwrap(),
    };
    current.rules.push(ContextualTrustRule {
        id: "later-unrelated-setting".into(),
        actor_ref: f.actor.clone(),
        predicate_ref: Some(f.cfg.predicate_ref.clone()),
        context_ref: Some(f.other_context.clone()),
        weight: 0.7,
    });
    f.space
        .memory
        .nexus()
        .system_session()
        .set_contextual_trust(DEFAULT_SPACE, 2, current)
        .await
        .unwrap();
    let f = reopen(f).await;
    let space = &f.space;
    let seq = space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let receipt = space
        .trust()
        .apply(
            auth(GOVERNOR),
            p.id.clone(),
            "recover persisted review".into(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.version, 2);
    // Only directory/cache hints may be repeated; no new native governance txn.
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
    let entries = space
        .memory
        .nexus()
        .governance()
        .read_audit(DEFAULT_SPACE, 100)
        .await
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|r| r.operation == "apply_trust_calibration")
            .count(),
        1
    );
    assert_eq!(space.trust().receipt(&p.id).await.unwrap(), Some(receipt));
    space.close().await.unwrap();
}

#[tokio::test]
async fn r8_caller_cancellation_during_native_audit_put_drains_before_space_close() {
    let (f, faults) = faulty("r8_owned").await;
    let p = two_errors(&f).await;
    let gate = FaultGate::new();
    faults.push_rule(FaultRule {
        kind: FaultKind::PauseBefore(gate.clone()),
        ..FaultRule::fail_once(FaultOp::Put, "gov_audit/")
    });
    let waiter = {
        let runtime = f.runtime.clone();
        let id = p.id.clone();
        tokio::spawn(async move {
            runtime
                .apply(auth(GOVERNOR), id, "approved owned mutation".into())
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_entered())
        .await
        .unwrap();
    waiter.abort();
    let _ = waiter.await;
    assert!(f.space.is_busy());
    let close = {
        let space = f.space.clone();
        tokio::spawn(async move { space.close().await })
    };
    sleep(Duration::from_millis(20)).await;
    assert!(!close.is_finished());
    gate.release();
    close.await.unwrap().unwrap();
    faults.reset();
    let f = reopen(f).await;
    let space = &f.space;
    let receipt = space
        .trust()
        .apply(auth(GOVERNOR), p.id, "replay after close".into())
        .await
        .unwrap();
    assert_eq!(receipt.version, 2);
    assert_eq!(
        space
            .memory
            .nexus()
            .governance()
            .read_audit(DEFAULT_SPACE, 100)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.operation == "apply_trust_calibration")
            .count(),
        1
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn r8_fact_intake_ack_loss_recovers_without_impersonating_the_verifier() {
    let (f, faults) = faulty("r8_intake").await;
    let input = verification(
        &f,
        0,
        "opaque-native-event",
        "opaque-root",
        VerificationCause::VerifiedFact,
        Some(false),
    );
    // A new target is saved first; the next target PUT acknowledges the native
    // Evidence + governed material. Lose only that final host checkpoint.
    faults.push_rule(FaultRule {
        skip: 1,
        ..FaultRule::fail_once(FaultOp::Put, "/targets/")
    });
    assert!(
        f.runtime
            .record_verification(auth(VERIFIER), input.clone())
            .await
            .is_err()
    );
    faults.reset();
    let f = reopen(f).await;
    let space = &f.space;
    let p = space.trust().propose(f.actor.clone()).await.unwrap();
    assert_eq!(p.independent_samples, 1);
    assert_eq!(p.evidence_refs.len(), 1);
    let evidence =
        crate::runtime_api::full_read(&space.memory.nexus().system_session(), &p.evidence_refs[0])
            .await
            .unwrap();
    let tx = space
        .memory
        .nexus()
        .store
        .find_transaction(evidence["_system"]["created_tx"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tx.origin["principal_id"], VERIFIER);
    let reference = space
        .trust()
        .record_verification(auth(VERIFIER), input.clone())
        .await
        .unwrap();
    assert_eq!(reference, p.evidence_refs[0]);
    let mut conflicting = input;
    conflicting.correct = Some(true);
    assert!(
        space
            .trust()
            .record_verification(auth(VERIFIER), conflicting)
            .await
            .is_err()
    );
    let store = f.app.object_store();
    let mut objects = store.list(None);
    while let Some(meta) = objects.try_next().await.unwrap() {
        if meta.location.as_ref().contains("/trust/") {
            let bytes = store
                .get(&meta.location)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let raw = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(!raw.contains("raw_measurement"));
            assert!(!raw.contains("opaque-native-event"));
            assert!(!raw.contains("opaque-root"));
        }
    }
    space.close().await.unwrap();
}

#[tokio::test]
async fn r8_trust_version_change_blocks_an_old_watch_and_frozen_action_context() {
    let p = policy(act());
    let executor = Arc::new(Executor::default());
    let f = fixture_with(
        "r8_basis",
        true,
        Arc::new(InMemory::new()),
        Some(bindings(p.clone(), executor.clone())),
    )
    .await;
    *p.context.lock().unwrap() = Some(ContextRequest {
        recall_receipt: None,
        anchor: f.propositions[0].clone(),
        required_refs: vec![],
        premises: vec![],
        applied_revisions: vec![],
        task_family: "memory.reminder".into(),
        environment_digest: pin("r3-env").digest,
        tool_versions: BTreeMap::from([("fixture".into(), "v1".into())]),
        deduplication_key: None,
    });
    let proposal = two_errors(&f).await;
    let watch=created_ref(&f.space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"trust basis",status:"disarmed",condition:{element: :actor}}}"#,kip::param("actor",f.actor.clone())).await;
    f.space
        .attention()
        .arm_watch(watch.clone(), 1)
        .await
        .unwrap();
    seed_kip(
        &f.space,
        kip::request_with(
            r#"UPDATE :actor SET FIELDS {name:"trigger"}"#,
            kip::param("actor", f.actor.clone()),
        ),
    )
    .await;
    let fire = f
        .space
        .memory
        .nexus()
        .session(auth(HOST))
        .advance_watch(DEFAULT_SPACE, &watch, 2, 1, 200)
        .await
        .unwrap();
    let wake = fire["wake_ref"].as_str().unwrap();
    let gate = process(&f.space, wake).await;
    assert_eq!(gate.decision.as_deref(), Some("act"), "{gate:?}");
    let child = dispatch_child(&f.space, wake).await;
    let other=created_ref(&f.space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"old trust",status:"disarmed",condition:{element: :actor}}}"#,kip::param("actor",f.actor.clone())).await;
    f.space
        .attention()
        .arm_watch(other.clone(), 1)
        .await
        .unwrap();
    f.runtime
        .apply(
            auth(GOVERNOR),
            proposal.id,
            "new qualified scoped trust".into(),
        )
        .await
        .unwrap();
    assert!(
        f.space
            .memory
            .nexus()
            .session(auth(HOST))
            .advance_watch(DEFAULT_SPACE, &other, 2, 1, 200)
            .await
            .is_err()
    );
    let status = process(&f.space, &child.wake_ref).await;
    assert_ne!(status.state, "completed", "{status:?}");
    assert!(executor.sends.lock().unwrap().is_empty());
    f.space.close().await.unwrap();
}
