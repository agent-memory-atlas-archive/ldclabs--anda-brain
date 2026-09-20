use super::*;
mod admission;
mod recovery;
use crate::consequence::trust::*;
use crate::runtime_api::{MemoryRuntimeBindings, SpaceRuntimeBindings};
use anda_cognitive_nexus::{
    governance::Permission,
    trust::{ContextualTrustRule, TrustConfiguration},
};
use anda_object_store::fault::{FaultGate, FaultHandle, FaultKind, FaultOp, FaultRule, FaultStore};
use futures::TryStreamExt;
use object_store::ObjectStore;

const PROPOSER: &str = "kip:principal:r8-proposer";
const VERIFIER: &str = "kip:principal:r8-independent-verifier";
const GOVERNOR: &str = "kip:principal:r8-governor";
struct Fixture {
    app: AppState,
    space: Arc<Space>,
    runtime: Arc<TrustRuntime>,
    cfg: TrustConfig,
    actor: String,
    context: String,
    other_context: String,
    claims: Vec<String>,
    propositions: Vec<String>,
    governor_grant: u64,
}
fn config(context: &str, apply: bool) -> TrustConfig {
    let mut c = TrustConfig {
        version: "verified-facts/mechanism-v1".into(),
        proposer_principal: PROPOSER.into(),
        governor_principal: Some(GOVERNOR.into()),
        observer: anda_kip::cognitive::ObserverControl {
            principal_id: VERIFIER.into(),
            configuration_digest: pin("independent-fact-instrument-v1").digest,
            control_domain: "independent-fixture-sensor".into(),
        },
        predicate_ref: format!("{PROFILE}prefers"),
        context_ref: context.into(),
        task_family: "fixture.source-facts.v1".into(),
        environment_digest: pin("fixed-fact-environment").digest,
        parameters: Some(TrustParameters {
            minimum_samples: 2,
            alpha: 0.05,
            gain: 1.0,
            step_cap: 0.8,
        }),
        calibration: None,
        automatic: false,
        apply,
        automatic_apply: false,
    };
    c.calibration = Some(TrustCalibration {
        reviewed_by: GOVERNOR.into(),
        contract_digest: c.contract_digest().unwrap(),
        approved: true,
        material: json!({"mechanism_fixture":true,"not_empirical_calibration":true}),
    });
    c
}
fn bindings_for(name: &str, cfg: TrustConfig) -> MemoryRuntimeBindings {
    MemoryRuntimeBindings {
        spaces: BTreeMap::from([(
            name.into(),
            SpaceRuntimeBindings {
                pin: pin("r8-host"),
                subjects: vec![],
                observers: vec![],
                actions: None,
                inbox: None,
                utility: None,
                trust: Some(cfg),
                semantic: None,
                #[cfg(feature = "learning")]
                learning: None,
                bootstrap: true,
                audience: Default::default(),
                inbox_recipient: None,
            },
        )]),
    }
}
async fn evict(app: &AppState) {
    for entry in app.spaces.read().await.values() {
        entry.last_access_ms.store(0, Ordering::Relaxed);
    }
    app.flush_and_evict_once(unix_ms(), 1).await;
    assert!(app.spaces.read().await.is_empty());
}
async fn fixture(name: &str, apply: bool) -> Fixture {
    fixture_with(name, apply, Arc::new(InMemory::new()), None).await
}
async fn fixture_with(
    name: &str,
    apply: bool,
    store: Arc<dyn ObjectStore>,
    actions: Option<ActionBindings>,
) -> Fixture {
    fixture_with_automatic(name, apply, store, actions, false).await
}
async fn fixture_with_automatic(
    name: &str,
    apply: bool,
    store: Arc<dyn ObjectStore>,
    actions: Option<ActionBindings>,
    automatic: bool,
) -> Fixture {
    let mut seed = test_app_state(name).fork_with_store(store.clone());
    seed.automatic = true;
    let space = create_loaded_space(&seed, name).await;
    let actor = created_ref(
        &space,
        r#"CREATE CONCEPT ?item {TYPE "Person" NAME "source actor"}"#,
        Default::default(),
    )
    .await;
    let context=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Event" NAME "work" SET ATTRIBUTES {summary:"registered fact domain"}}"#,Default::default()).await;
    let other_context=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Event" NAME "home" SET ATTRIBUTES {summary:"another domain"}}"#,Default::default()).await;
    let mut claims = vec![];
    let mut propositions = vec![];
    for n in 0..3 {
        let object = created_ref(
            &space,
            r#"CREATE CONCEPT ?item {TYPE "Preference" NAME :name}"#,
            kip::param("name", format!("fact-{n}")),
        )
        .await;
        let p = created_ref(
            &space,
            r#"ENSURE PROPOSITION ?item (:actor,"prefers",:object)"#,
            serde_json::from_value(json!({"actor":actor,"object":object})).unwrap(),
        )
        .await;
        let a=created_ref(&space,r#"CREATE ASSERTION ?item {SET FIELDS {proposition: :p,asserted_by: :actor,mode:"stated",stance:"support",confidence:1,context_refs:[{id: :context}]}}"#,serde_json::from_value(json!({"p":p,"actor":actor,"context":context})).unwrap()).await;
        created_ref(&space,r#"CREATE ASSERTION ?item {SET FIELDS {proposition: :p,asserted_by: :actor,mode:"stated",stance:"support",confidence:1}}"#,serde_json::from_value(json!({"p":p,"actor":actor})).unwrap()).await;
        claims.push(a);
        propositions.push(p);
    }
    drop(space);
    evict(&seed).await;
    let mut cfg = config(&context, apply);
    cfg.automatic = automatic;
    cfg.automatic_apply = automatic && apply;
    let mut app = test_app_state(name).fork_with_store(store);
    app.automatic = true;
    if let Some(actions) = actions {
        app = app.with_action_bindings(actions).unwrap();
    }
    let app = app
        .with_memory_runtime_bindings(bindings_for(name, cfg.clone()))
        .unwrap();
    let space = app.load_space_with(name, false, false).await.unwrap();
    // Bootstrap MUST NOT grant governance power merely from a configured name.
    assert!(!space.trust().status().await.governor_authorized);
    let governor_grant = principal(
        &space,
        GOVERNOR,
        &[
            "read",
            "read_history",
            "read_governance_history",
            "manage_trust",
        ],
    )
    .await;
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
fn verification(
    f: &Fixture,
    index: usize,
    event: &str,
    root: &str,
    cause: VerificationCause,
    correct: Option<bool>,
) -> TrustVerificationInput {
    TrustVerificationInput {
        event_key: event.into(),
        root_key: root.into(),
        assertion_ref: f.claims[index].clone(),
        assessed_at: anda_cognitive_nexus::time::now(),
        cause,
        correct,
        material: json!({"independent_fixture_replay":event,"raw_measurement":"source value differs from separately observed fixture state","not_an_action_success_flag":true}),
    }
}
async fn record(f: &Fixture, index: usize, event: &str, root: &str, correct: bool) -> String {
    f.runtime
        .record_verification(
            auth(VERIFIER),
            verification(
                f,
                index,
                event,
                root,
                VerificationCause::VerifiedFact,
                Some(correct),
            ),
        )
        .await
        .unwrap()
}
async fn two_errors(f: &Fixture) -> TrustProposal {
    record(f, 0, "fact-a", "root-a", false).await;
    record(f, 1, "fact-b", "root-b", false).await;
    f.runtime.propose(f.actor.clone()).await.unwrap()
}
async fn trust(f: &Fixture) -> anda_cognitive_nexus::store::rows::ControlRecordRow {
    f.space
        .memory
        .nexus()
        .system_session()
        .read_control(DEFAULT_SPACE, "trust", None)
        .await
        .unwrap()
        .unwrap()
}
async fn belief(f: &Fixture, context: Option<&str>, history: Option<u64>) -> Json {
    let suffix = history
        .map(|n| format!(" AS OF SEQ {n}"))
        .unwrap_or_default();
    let request = kip::request_with(
        format!(
            "FIND(?b) WHERE {{?p PROPOSITION(id: :id) ?b BELIEF(?p)}}{suffix} WITH EPISTEMIC {{context_refs: :contexts}}"
        ),
        serde_json::from_value(
            json!({"id":f.propositions[0],"contexts":context.into_iter().collect::<Vec<_>>()}),
        )
        .unwrap(),
    );
    let response = f.space.execute_kip_readonly(request).await.unwrap();
    assert!(kip::succeeded(&response), "{response:?}");
    kip::ok_result(&response).unwrap()[0].clone()
}
async fn audits(f: &Fixture) -> usize {
    f.space
        .memory
        .nexus()
        .governance()
        .read_audit(DEFAULT_SPACE, 100)
        .await
        .unwrap()
        .iter()
        .filter(|r| r.operation == "apply_trust_calibration")
        .count()
}

#[tokio::test]
async fn r8_proposals_do_not_write_trust_and_nonfactual_failures_do_not_penalize_sources() {
    let f = fixture("r8_proposals", false).await;
    for (n, cause) in [
        VerificationCause::ExecutionFailure,
        VerificationCause::MissingPrecondition,
        VerificationCause::EnvironmentChange,
        VerificationCause::Unknown,
    ]
    .into_iter()
    .enumerate()
    {
        f.runtime
            .record_verification(
                auth(VERIFIER),
                verification(
                    &f,
                    0,
                    &format!("failure-{n}"),
                    &format!("root-{n}"),
                    cause,
                    Some(false),
                ),
            )
            .await
            .unwrap();
    }
    let before = trust(&f).await;
    let p = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert_eq!(p.independent_samples, 0);
    assert!(p.new_weight.is_none());
    assert_eq!(p.excluded.len(), 4);
    assert_eq!(trust(&f).await.version, before.version);
    let p = two_errors(&f).await;
    assert_eq!(p.independent_samples, 2);
    assert!(p.new_weight.is_some());
    assert!(p.native_proposal.is_some());
    assert!(
        f.runtime
            .apply(auth(GOVERNOR), p.id, "not enabled".into())
            .await
            .is_err()
    );
    assert_eq!(trust(&f).await.version, before.version);
    assert_eq!(audits(&f).await, 0);
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_scoped_application_preserves_global_trust_confidence_and_historical_belief() {
    let f = fixture("r8_scope", true).await;
    let p = two_errors(&f).await;
    let old_seq = f
        .space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let before = belief(&f, Some(&f.context), None).await;
    assert!(
        f.space
            .memory
            .nexus()
            .session(auth(GOVERNOR))
            .effective_authority(DEFAULT_SPACE)
            .await
            .unwrap()
            .authorize(Permission::Create, &Default::default(), &auth(GOVERNOR))
            .into_result()
            .is_err()
    );
    let confidence =
        crate::runtime_api::full_read(&f.space.memory.nexus().system_session(), &f.claims[0])
            .await
            .unwrap()["confidence"]
            .clone();
    f.space
        .miss_cache
        .record_miss("trust-sensitive-query", unix_ms())
        .await
        .unwrap();
    let r = f
        .runtime
        .apply(
            auth(GOVERNOR),
            p.id.clone(),
            "Reviewed independent factual errors in this domain only".into(),
        )
        .await
        .unwrap();
    assert_eq!(r.version, p.expected_version + 1);
    assert_eq!(r.governor, GOVERNOR);
    assert_eq!(audits(&f).await, 1);
    let stored = trust(&f).await;
    assert_eq!(stored.value["weights"], json!({}));
    assert_eq!(stored.value["default_weight"], 1.0);
    assert_eq!(stored.value["rules"].as_array().unwrap().len(), 1);
    assert_eq!(stored.value["rules"][0]["context_ref"], f.context);
    assert_eq!(belief(&f, None, None).await["status"], "accepted");
    assert_eq!(
        belief(&f, Some(&f.other_context), None).await["status"],
        "accepted"
    );
    let current = belief(&f, Some(&f.context), None).await;
    assert_eq!(current["status"], "uncertain");
    assert_ne!(
        before["basis"]["trust_version"],
        current["basis"]["trust_version"]
    );
    assert_eq!(
        belief(&f, Some(&f.context), Some(old_seq)).await["status"],
        before["status"]
    );
    assert_eq!(
        crate::runtime_api::full_read(&f.space.memory.nexus().system_session(), &f.claims[0])
            .await
            .unwrap()["confidence"],
        confidence
    );
    assert!(
        !f.space
            .miss_cache
            .is_fresh_miss("trust-sensitive-query", unix_ms())
            .await
            .unwrap()
    );
    assert_eq!(
        f.runtime
            .apply(auth(GOVERNOR), p.id.clone(), "replayed review".into())
            .await
            .unwrap(),
        r
    );
    assert_eq!(audits(&f).await, 1);
    // Re-announcing an applied root does not apply another bounded step.
    for e in &p.evidence_refs {
        f.runtime.enqueue(e.clone()).await.unwrap();
    }
    let again = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert!(again.new_weight.is_none());
    assert_eq!(trust(&f).await.version, r.version);
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_duplicate_roots_and_repeated_verification_of_one_claim_never_inflate_samples() {
    let f = fixture("r8_roots", true).await;
    let first = record(&f, 0, "first", "same-root", false).await;
    record(&f, 1, "copy", "same-root", false).await;
    record(
        &f,
        0,
        "independent-check-of-the-same-claim",
        "different-root",
        false,
    )
    .await;
    let p = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert_eq!(p.independent_samples, 1);
    assert!(p.new_weight.is_none());
    assert_eq!(p.evidence_refs, vec![first]);
    assert_eq!(
        p.excluded
            .values()
            .filter(|v| v.as_str() == "duplicate_evidence_root")
            .count(),
        2
    );
    record(&f, 2, "new-fact", "new-root", false).await;
    let p = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert_eq!(p.independent_samples, 2);
    assert!(p.new_weight.is_some());
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_governance_requires_real_current_manage_trust_and_rejects_semantic_impersonation() {
    let f = fixture("r8_authority", true).await;
    let p = two_errors(&f).await;
    for who in [
        PROPOSER,
        VERIFIER,
        f.actor.as_str(),
        "kip:principal:ordinary-reader",
    ] {
        assert!(
            f.runtime
                .apply(auth(who), p.id.clone(), "claimed approval".into())
                .await
                .is_err()
        );
    }
    f.space
        .memory
        .nexus()
        .governance()
        .revoke_grant(f.governor_grant, "kip:principal:system")
        .await
        .unwrap();
    assert!(!f.runtime.status().await.governor_authorized);
    assert!(
        f.runtime
            .apply(auth(GOVERNOR), p.id, "revoked governor".into())
            .await
            .is_err()
    );
    assert_eq!(trust(&f).await.version, 1);
    assert_eq!(audits(&f).await, 0);
    assert!(
        f.runtime
            .record_verification(
                auth(PROPOSER),
                verification(
                    &f,
                    0,
                    "fake",
                    "fake",
                    VerificationCause::VerifiedFact,
                    Some(false)
                )
            )
            .await
            .is_err()
    );
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_version_conflict_preserves_other_settings_until_explicit_fresh_review() {
    let f = fixture("r8_cas", true).await;
    let p = two_errors(&f).await;
    let other = ContextualTrustRule {
        id: "neighbor-rule".into(),
        actor_ref: f.actor.clone(),
        predicate_ref: Some(f.cfg.predicate_ref.clone()),
        context_ref: Some(f.other_context.clone()),
        weight: 0.7,
    };
    f.space
        .memory
        .nexus()
        .system_session()
        .set_contextual_trust(
            DEFAULT_SPACE,
            1,
            TrustConfiguration {
                weights: BTreeMap::new(),
                default_weight: 1.0,
                rules: vec![other.clone()],
            },
        )
        .await
        .unwrap();
    assert!(
        f.runtime
            .apply(auth(GOVERNOR), p.id, "stale review".into())
            .await
            .is_err()
    );
    assert_eq!(trust(&f).await.version, 2);
    assert_eq!(trust(&f).await.value["rules"], json!([other.clone()]));
    let fresh = f.runtime.propose(f.actor.clone()).await.unwrap();
    assert_eq!(fresh.expected_version, 2);
    let r = f
        .runtime
        .apply(
            auth(GOVERNOR),
            fresh.id,
            "Fresh review preserves neighbor settings".into(),
        )
        .await
        .unwrap();
    assert_eq!(r.version, 3);
    let rules: Vec<ContextualTrustRule> =
        serde_json::from_value(trust(&f).await.value["rules"].clone()).unwrap();
    assert!(rules.contains(&other));
    f.space.close().await.unwrap();
}

#[tokio::test]
async fn r8_corrected_or_purged_verification_cannot_authorize_a_pending_update() {
    for purge in [false, true] {
        let name = if purge { "r8_purge" } else { "r8_correction" };
        let f = fixture(name, true).await;
        let p = two_errors(&f).await;
        let original = p.evidence_refs[0].clone();
        if purge {
            seed_kip(
                &f.space,
                kip::request_with("PURGE :id CONFIRM \"PURGE\"", kip::param("id", original)),
            )
            .await;
        } else {
            let replacement=created_ref(&f.space,r#"CREATE EVIDENCE ?item {SET FIELDS {evidence_class:"observation",payload:"independent correction"}}"#,Default::default()).await;
            seed_kip(
                &f.space,
                kip::request_with(
                    "TRANSITION :id TO \"corrected\" BY :replacement",
                    serde_json::from_value(json!({"id":original,"replacement":replacement}))
                        .unwrap(),
                ),
            )
            .await;
        }
        assert!(
            f.runtime
                .apply(auth(GOVERNOR), p.id, "outdated facts".into())
                .await
                .is_err()
        );
        assert_eq!(trust(&f).await.version, 1);
        assert_eq!(audits(&f).await, 0);
        f.space.close().await.unwrap();
    }
}

#[tokio::test]
async fn r8_restore_appends_new_control_and_reason_without_erasing_history() {
    let f = fixture("r8_restore", true).await;
    let p = two_errors(&f).await;
    let applied = f
        .runtime
        .apply(auth(GOVERNOR), p.id.clone(), "initial scoped review".into())
        .await
        .unwrap();
    let correction=created_ref(&f.space,r#"CREATE EVIDENCE ?item {SET FIELDS {evidence_class:"observation",payload:"operator verified calibration input correction"}}"#,Default::default()).await;
    let restore = f
        .runtime
        .propose_restore(
            auth(GOVERNOR),
            p.id.clone(),
            "Restore previous scope after independent correction review".into(),
            vec![correction],
        )
        .await
        .unwrap();
    assert_eq!(restore.restores.as_deref(), Some(p.id.as_str()));
    assert_eq!(restore.independent_samples, 0);
    let receipt = f
        .runtime
        .apply(
            auth(GOVERNOR),
            restore.id,
            "approved scoped restoration".into(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.version, applied.version + 1);
    assert!(
        trust(&f).await.value["rules"]
            .as_array()
            .is_none_or(|rules| rules.is_empty())
    );
    assert_eq!(audits(&f).await, 2);
    assert_eq!(f.runtime.receipt(&p.id).await.unwrap(), Some(applied));
    assert_eq!(
        belief(&f, Some(&f.context), None).await["status"],
        "accepted"
    );
    f.space.close().await.unwrap();
}
