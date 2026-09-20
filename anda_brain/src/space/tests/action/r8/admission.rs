use super::*;

#[tokio::test]
async fn r8_unknown_parameters_and_unreviewed_methods_remain_suggestions() {
    let mut f = fixture("r8_parameters", false).await;
    f.cfg.parameters = None;
    f.cfg.calibration = None;
    let f = recovery::reopen(f).await;
    let p = two_errors(&f).await;
    assert_eq!(p.independent_samples, 2);
    assert!(p.new_weight.is_none());
    assert!(p.native_proposal.is_none());
    assert_eq!(
        p.reason.as_deref(),
        Some("trust_mapping_parameters_missing")
    );
    assert_eq!(trust(&f).await.version, 1);
    let mut c = f.cfg.clone();
    c.apply = true;
    assert!(c.validate().is_err());
    let mut c = config(&f.context, false);
    c.calibration = None;
    c.apply = true;
    assert!(c.validate().is_err());
    let mut c = config(&f.context, false);
    c.parameters.as_mut().unwrap().alpha = 0.0;
    assert!(c.validate().is_err());
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_source_scope_self_verification_and_missing_commitments_are_rejected() {
    let f = fixture("r8_claims", true).await;
    let foreign=created_ref(&f.space,r#"CREATE ASSERTION ?item {SET FIELDS {proposition: :p,asserted_by: :actor,mode:"stated",stance:"support",confidence:1,context_refs:[{id: :context}]}}"#,serde_json::from_value(json!({"p":f.propositions[0],"actor":f.actor,"context":f.other_context})).unwrap()).await;
    let mut input = verification(
        &f,
        0,
        "wrong-scope",
        "wrong-scope",
        VerificationCause::VerifiedFact,
        Some(false),
    );
    input.assertion_ref = foreign;
    assert!(
        f.runtime
            .record_verification(auth(VERIFIER), input)
            .await
            .is_err()
    );
    let mut input = verification(
        &f,
        0,
        "raw-tuple",
        "raw-tuple",
        VerificationCause::VerifiedFact,
        Some(false),
    );
    input.assertion_ref = f.propositions[0].clone();
    assert!(
        f.runtime
            .record_verification(auth(VERIFIER), input)
            .await
            .is_err()
    );
    principal(
        &f.space,
        VERIFIER,
        &["assert", "record_attributed_assertion"],
    )
    .await;
    let session = f.space.memory.nexus().session(auth(VERIFIER));
    let response=anda_kip::execute_request(&session,&kip::request_with(r#"CREATE ASSERTION ?a {SET FIELDS {proposition: :p,asserted_by: :actor,mode:"stated",stance:"support",confidence:1,context_refs:[{id: :context}]}}"#,serde_json::from_value(json!({"p":f.propositions[0],"actor":f.actor,"context":f.context})).unwrap())).await;
    assert!(kip::succeeded(&response), "{response:?}");
    let mut input = verification(
        &f,
        0,
        "self-check",
        "self-root",
        VerificationCause::VerifiedFact,
        Some(false),
    );
    input.assertion_ref = kip::ok_result(&response).unwrap()["handles"]["a"]
        .as_str()
        .unwrap()
        .into();
    assert!(
        f.runtime
            .record_verification(auth(VERIFIER), input)
            .await
            .is_err()
    );
    assert_eq!(trust(&f).await.version, 1);
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_abandoned_review_releases_only_uncommitted_roots_for_fresh_explicit_review() {
    let f = fixture("r8_abandon", true).await;
    let p = two_errors(&f).await;
    f.runtime
        .abandon(
            auth(GOVERNOR),
            p.id.clone(),
            "operator requests a fresh review".into(),
        )
        .await
        .unwrap();
    assert!(
        f.runtime
            .apply(auth(GOVERNOR), p.id.clone(), "old review".into())
            .await
            .is_err()
    );
    let new = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert_ne!(new.id, p.id);
    assert_eq!(new.expected_version, p.expected_version);
    let receipt = f
        .runtime
        .apply(
            auth(GOVERNOR),
            new.id.clone(),
            "fresh authenticated review".into(),
        )
        .await
        .unwrap();
    assert!(
        f.runtime
            .abandon(
                auth(GOVERNOR),
                new.id,
                "cannot erase applied governance".into()
            )
            .await
            .is_err()
    );
    assert_eq!(trust(&f).await.version, receipt.version);
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_automatic_application_requires_explicit_switches_and_native_governor_grant() {
    let mut f = fixture("r8_automatic", true).await;
    f.cfg.automatic = true;
    f.cfg.automatic_apply = true;
    let f = recovery::reopen(f).await;
    two_errors(&f).await;
    assert!(f.runtime.status().await.automatic_apply);
    f.app.attention_tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if trust(&f).await.version == 2 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(audits(&f).await, 1);
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_catalog_ack_repair_keeps_newer_verified_sources() {
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let f = fixture_with("r8_catalog", true, Arc::new(store), None).await;
    faults.push_rule(FaultRule {
        skip: 1,
        ..FaultRule::fail_once(FaultOp::Put, "/catalog")
    });
    let input = verification(
        &f,
        0,
        "interrupted-first",
        "first",
        VerificationCause::VerifiedFact,
        Some(false),
    );
    assert!(
        f.runtime
            .record_verification(auth(VERIFIER), input.clone())
            .await
            .is_err()
    );
    faults.reset();
    let b = record(&f, 1, "later-second", "second", false).await;
    let c = record(&f, 2, "later-third", "third", false).await;
    let pending = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert!(pending.new_weight.is_none());
    assert_eq!(
        pending.reason.as_deref(),
        Some("unresolved_verification_intake_or_material")
    );
    // An authenticated retry completes the old intent; repair must preserve
    // later source updates rather than reinstall its stale target snapshot.
    f.runtime
        .record_verification(auth(VERIFIER), input)
        .await
        .unwrap();
    let p = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert_eq!(p.independent_samples, 2);
    assert_eq!(p.evidence_refs, vec![b, c]);
    assert_eq!(f.runtime.proposals(0, 32).await.unwrap().0.len(), 1);
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_r4_outcomes_discover_independent_fact_records_without_using_action_scores() {
    use crate::consequence::*;
    let policy = policy(act());
    let executor = Arc::new(Executor::default());
    let f = fixture_with_automatic(
        "r8_r4",
        false,
        Arc::new(InMemory::new()),
        Some(bindings(policy.clone(), executor)),
        true,
    )
    .await;
    *policy.context.lock().unwrap() = Some(ContextRequest {
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
    // A separately owned producer journal exercises discovery from the native
    // R4 announcement instead of relying on the consumer's local admission map.
    let producer = TrustRuntime::new(
        f.space.memory.nexus(),
        Arc::new(InMemory::new()),
        f.space.recall_receipts(),
        Some(f.cfg.clone()),
        false,
    );
    assert!(f.runtime.proposals(0, 32).await.unwrap().0.is_empty());
    for index in 0..2 {
        let reference = producer
            .record_verification(
                auth(VERIFIER),
                verification(
                    &f,
                    index,
                    &format!("producer-{index}"),
                    &format!("producer-root-{index}"),
                    VerificationCause::VerifiedFact,
                    Some(false),
                ),
            )
            .await
            .unwrap();
        let watch=created_ref(&f.space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"announce verification",status:"disarmed",condition:{element: :actor}}}"#,kip::param("actor",f.actor.clone())).await;
        f.space
            .attention()
            .arm_watch(watch.clone(), 1)
            .await
            .unwrap();
        seed_kip(
            &f.space,
            kip::request_with(
                "UPDATE :actor SET FIELDS {name: :name}",
                serde_json::from_value(json!({"actor":f.actor,"name":format!("event-{index}")}))
                    .unwrap(),
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
        let child = dispatch_child(&f.space, wake).await;
        process(&f.space, &child.wake_ref).await;
        let attempt = gate.attempt_ref.as_deref().unwrap();
        let c = ConsequenceRuntime::new(
            f.space.memory.nexus(),
            f.app.attention_directory.clone(),
            child.scope.clone(),
            vec![ObserverContract {
                principal_id: VERIFIER.into(),
                configuration_digest: f.cfg.observer.configuration_digest.clone(),
                control_domain: f.cfg.observer.control_domain.clone(),
                task_family: "memory.reminder".into(),
                metric: "delivery".into(),
                window: "fact-announcement".into(),
                maximum_delay_ms: 60_000,
            }],
            false,
            #[cfg(feature = "learning")]
            Some(Arc::downgrade(&f.space.learning())),
        );
        c.submit(
            auth(VERIFIER),
            OutcomeInput {
                utility: None,
                space_instance: child.scope.space_instance,
                attempt_ref: attempt.into(),
                observer_configuration_digest: f.cfg.observer.configuration_digest.clone(),
                event_key: format!("announcement-{index}"),
                observed_at: anda_cognitive_nexus::time::now(),
                metric: "delivery".into(),
                window: "fact-announcement".into(),
                observation: Observation::Measurement {
                    terminal: true,
                    outcome_status: OutcomeStatus::Success,
                    magnitude: Some(1.0),
                    payload: json!({"trust_verification_ref":reference}),
                },
                correction_of: None,
                safety_signal: None,
            },
        )
        .await
        .unwrap();
    }
    f.app.attention_tick().await.unwrap();
    let proposal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let page = f.runtime.proposals(0, 32).await.unwrap();
            if let Some(p) = page.0.into_iter().next() {
                break p;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(proposal.independent_samples, 2);
    assert!(
        proposal.new_weight.unwrap() < 0.21,
        "successful deliveries must not override independently verified factual errors"
    );
    assert_eq!(trust(&f).await.version, 1);
    producer.shutdown().await;
    f.space.close().await.unwrap();
}

#[test]
fn r8_template_has_no_statistical_defaults_or_implicit_governance_enablement() {
    let config: crate::runtime_api::config::RuntimeConfig =
        serde_json::from_str(include_str!("../../../../../trust.runtime.example.json")).unwrap();
    let bindings = config.resolve(|_| None).unwrap();
    let trust = bindings
        .spaces
        .values()
        .next()
        .unwrap()
        .trust
        .as_ref()
        .unwrap();
    assert!(trust.parameters.is_none());
    assert!(trust.calibration.is_none());
    assert!(!trust.automatic && !trust.apply && !trust.automatic_apply);
    let mut invalid = trust.clone();
    invalid.automatic_apply = true;
    assert!(invalid.validate().is_err());
    let mut invalid = trust.clone();
    invalid.governor_principal = Some(invalid.observer.principal_id.clone());
    assert!(invalid.validate().is_err());
}
