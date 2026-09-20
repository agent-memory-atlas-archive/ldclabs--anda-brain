use super::*;
use anda_cognitive_nexus::{
    governance::store::{GrantDraft, PrincipalDraft},
    nexus::DEFAULT_SPACE,
    profiles::{COGNITIVE_MEMORY, COGNITIVE_MEMORY_ID, COGNITIVE_MEMORY_VERSION},
    schema::{PackageState, SchemaLock, SchemaPackage},
};
use anda_db::database::AndaDB;
use object_store::memory::InMemory;

const OBSERVER: &str = "kip:principal:native-observer";

pub(super) async fn command(nexus: &CognitiveNexus, text: &str) -> Json {
    let result = anda_kip::execute_request(nexus, &Request::single(text)).await;
    successful(&result).unwrap().clone()
}

type Fixture = (
    Arc<AndaDB>,
    Arc<CognitiveNexus>,
    NativeLearning,
    PairedTrialPlan,
    Json,
    String,
);

pub(super) async fn setup() -> Fixture {
    setup_with_store(Arc::new(InMemory::new())).await
}

async fn setup_with_store(storage: Arc<InMemory>) -> Fixture {
    let db = Arc::new(
        AndaDB::connect(storage, crate::testkit::db_config("native_learning"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    nexus
        .install_package(&SchemaPackage::parse(COGNITIVE_MEMORY).unwrap(), "test")
        .await
        .unwrap();
    let mut lock = SchemaLock::default();
    lock.packages
        .insert(COGNITIVE_MEMORY_ID.into(), COGNITIVE_MEMORY_VERSION.into());
    lock.states
        .insert(COGNITIVE_MEMORY_ID.into(), PackageState::Active);
    nexus.activate_schema(DEFAULT_SPACE, lock).await.unwrap();
    command(
        &nexus,
        r#"MUTATE {
        CREATE CONCEPT ?p {TYPE "Person" NAME "Test actor"}
        CREATE CONCEPT ?v {TYPE "Preference" NAME "Test preference"}
        ENSURE PROPOSITION ?fact (?p,"prefers",?v)
    }"#,
    )
    .await;
    let behavior =
        json!({"task_family":"tool_workflow.precondition.v1","procedure":"prepare then commit"});
    let skill = command(&nexus, &format!(r#"MUTATE {{
        CREATE CONCEPT ?s {{TYPE "Skill" SET ATTRIBUTES {{skill_class:"workflow",summary:"prepare then commit",status:"proposed"}} SET STRUCTURAL {{("current_revision",?r)}}}}
        CREATE CONCEPT ?r {{TYPE "SkillRevision" SET ATTRIBUTES {{task_family:"tool_workflow.precondition.v1",procedure:"prepare then commit",behavior_digest:"{}"}} SET STRUCTURAL {{("revision_of",?s)}}}}
    }}"#, content_digest(&behavior).unwrap())).await;
    let mut plan = super::super::tests::plan(2);
    plan.candidate_revision = skill["handles"]["r"].as_str().unwrap().into();
    nexus
        .governance()
        .ensure_principal(PrincipalDraft {
            principal_id: OBSERVER.into(),
            principal_class: "service".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    nexus
        .governance()
        .create_grant(
            GrantDraft {
                space_id: DEFAULT_SPACE.into(),
                grantee_principal: OBSERVER.into(),
                actions: vec![
                    "discover".into(),
                    "read".into(),
                    "create".into(),
                    "record_outcome".into(),
                    "derive".into(),
                    "read_history".into(),
                ],
                ..Default::default()
            },
            SYSTEM_PRINCIPAL,
        )
        .await
        .unwrap();
    let observer = ObserverControl {
        principal_id: OBSERVER.into(),
        configuration_digest: plan.execution.observer_configuration_digest().unwrap(),
        control_domain: "independent-native-test".into(),
    };
    let native = NativeLearning::new(
        nexus.clone(),
        DEFAULT_SPACE.into(),
        AuthContext::system(),
        observer,
    )
    .unwrap();
    let basis = command(
        &nexus,
        r#"FIND(?b) WHERE { ?p PROPOSITION (id:"P-1") ?b BELIEF (?p) }"#,
    )
    .await[0]["basis"]
        .clone();
    (
        db,
        nexus,
        native,
        plan,
        basis,
        skill["handles"]["s"].as_str().unwrap().into(),
    )
}

fn intent(key: &str, operation: NativeOperation) -> NativeRequest {
    NativeRequest {
        space_id: DEFAULT_SPACE.into(),
        idempotency_key: key.into(),
        operation,
    }
}

pub(super) async fn install(native: &NativeLearning, plan: &PairedTrialPlan) -> FrozenNativePlan {
    let request = intent(
        "install",
        NativeOperation::InstallPlan(InstallPlanInput {
            plan: plan.clone(),
            policy_id: "native-policy".into(),
            policy_version: "1".into(),
            expected_policy_version: 0,
            allowed_parameters: vec![],
        }),
    );
    let first = native.execute(&request, None).await.unwrap();
    let replay = native.execute(&request, None).await.unwrap();
    assert_eq!(first.frozen_plan, replay.frozen_plan);
    first.frozen_plan.unwrap()
}

pub(super) fn attempt_request(
    plan: &PairedTrialPlan,
    basis: &Json,
    pair: &str,
    arm: NativeArm,
) -> NativeRequest {
    let label = if matches!(arm, NativeArm::Baseline) {
        "baseline"
    } else {
        "treatment"
    };
    let key = format!("{label}-{pair}");
    intent(
        &key,
        NativeOperation::Attempt(AttemptInput {
            decision: DecisionInput {
                plan: plan.clone(),
                pair_id: pair.into(),
                applied_revision_version: matches!(arm, NativeArm::Treatment { .. }).then_some(1),
                origin: None,
                arm,
                basis: basis.clone(),
                context_pin: NativeContextPin {
                    id: "P-1".into(),
                    version: 1,
                },
            },
            attempt_id: key.clone(),
            started_at: anda_cognitive_nexus::time::now(),
        }),
    )
}

pub(super) fn outcome_request(
    plan: &PairedTrialPlan,
    frozen: &FrozenNativePlan,
    pair: &str,
    arm: NativeArm,
    receipt: &NativeReceipt,
) -> NativeRequest {
    let attempt = receipt.handles["attempt"].clone();
    intent(
        &format!("outcome-{attempt}"),
        NativeOperation::Outcome(OutcomeInput {
            plan: plan.clone(),
            frozen: frozen.clone(),
            pair_id: pair.into(),
            arm,
            decision_ref: receipt.handles["decision"].clone(),
            attempt_ref: attempt.clone(),
            observation_key: format!("result-{attempt}"),
            observed_at: anda_cognitive_nexus::time::now(),
            outcome_status: NativeOutcomeStatus::Success,
            payload: json!({"verified":true,"costs":{"tool_calls":2}}),
        }),
    )
}

#[tokio::test]
async fn native_records_recover_original_handles_and_do_not_change_skill_standing() {
    let (db, nexus, native, plan, basis, skill) = setup().await;
    let frozen = install(&native, &plan).await;
    let observer = AuthContext::principal(OBSERVER);
    native.validate_observer(&observer, &frozen).await.unwrap();
    let mut attempts = vec![];
    let mut outcomes = vec![];
    for pair in plan.pairs.keys() {
        let request = attempt_request(&plan, &basis, pair, NativeArm::Baseline);
        assert!(native.reconcile(&request, None).await.unwrap().is_none());
        let receipt = native.execute(&request, None).await.unwrap();
        // Simulate a successful native commit with a lost host state write by
        // discarding the response and deserializing the persisted original intent.
        let restored: NativeRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert!(
            native
                .clone()
                .reconcile(&restored, None)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            native.execute(&restored, None).await.unwrap().handles,
            receipt.handles
        );
        let outcome = outcome_request(&plan, &frozen, pair, NativeArm::Baseline, &receipt);
        let observed = native.execute(&outcome, Some(&observer)).await.unwrap();
        assert_eq!(
            native
                .execute(&outcome, Some(&observer))
                .await
                .unwrap()
                .handles,
            observed.handles
        );
        attempts.push(receipt.handles["attempt"].clone());
        outcomes.push(observed.handles["outcome"].clone());
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
    let request = intent("trial", NativeOperation::OpenTrial(input));
    let trial = native.execute(&request, None).await.unwrap();
    let sequence = nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq;
    assert_eq!(
        native
            .recover_committed(&request, None)
            .await
            .unwrap()
            .unwrap()
            .handles,
        trial.handles
    );
    assert_eq!(
        nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq,
        sequence
    );
    assert_eq!(
        native.execute(&request, None).await.unwrap().handles,
        trial.handles
    );
    let request = attempt_request(
        &plan,
        &basis,
        plan.pairs.keys().next().unwrap(),
        NativeArm::Treatment {
            trial_ref: trial.handles["trial"].clone(),
        },
    );
    let treatment = native.execute(&request, None).await.unwrap();
    assert!(
        treatment.handles.contains_key("decision") && treatment.handles.contains_key("attempt")
    );
    let row = nexus
        .store
        .get_element(skill.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(
        anda_cognitive_nexus::view::render(&row)["attributes"]["status"],
        "proposed"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn duplicate_changed_and_foreign_outcomes_do_not_create_extra_successes() {
    let (db, _, native, plan, basis, _) = setup().await;
    let frozen = install(&native, &plan).await;
    let pair = plan.pairs.keys().next().unwrap();
    let request = attempt_request(&plan, &basis, pair, NativeArm::Baseline);
    let receipt = native.execute(&request, None).await.unwrap();
    let mut changed = request.clone();
    if let NativeOperation::Attempt(input) = &mut changed.operation {
        input.attempt_id.push_str("-changed");
    }
    assert_eq!(
        native.execute(&changed, None).await.unwrap_err().code,
        KipErrorCode::IdempotencyConflict
    );
    let observer = AuthContext::principal(OBSERVER);
    let outcome = outcome_request(&plan, &frozen, pair, NativeArm::Baseline, &receipt);
    assert!(native.execute(&outcome, None).await.is_err());
    assert!(
        native
            .execute(&outcome, Some(&AuthContext::system()))
            .await
            .is_err()
    );
    let mut delegated = observer.clone();
    delegated.delegation_chain.push("kip:delegation:1".into());
    assert!(native.execute(&outcome, Some(&delegated)).await.is_err());
    native.execute(&outcome, Some(&observer)).await.unwrap();
    let mut duplicate = outcome.clone();
    duplicate.idempotency_key.push_str("-other-key");
    assert!(native.execute(&duplicate, Some(&observer)).await.is_err());
    let mut foreign = outcome.clone();
    foreign.space_id = "kip:space:other".into();
    assert!(native.execute(&foreign, Some(&observer)).await.is_err());
    let mut wrong = outcome.clone();
    if let NativeOperation::Outcome(input) = &mut wrong.operation {
        input.plan.candidate_revision = "C-9999".into();
    }
    assert!(native.execute(&wrong, Some(&observer)).await.is_err());
    db.close().await.unwrap();
}

#[tokio::test]
async fn current_policy_and_real_observer_grants_are_required() {
    let (db, nexus, native, plan, basis, _) = setup().await;
    let frozen = install(&native, &plan).await;
    let pair = plan.pairs.keys().next().unwrap();
    let receipt = native
        .execute(
            &attempt_request(&plan, &basis, pair, NativeArm::Baseline),
            None,
        )
        .await
        .unwrap();
    let outcome = outcome_request(&plan, &frozen, pair, NativeArm::Baseline, &receipt);
    let ungranted = "kip:principal:ungranted-observer";
    nexus
        .governance()
        .ensure_principal(PrincipalDraft {
            principal_id: ungranted.into(),
            principal_class: "service".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut control = native.observer.clone();
    control.principal_id = ungranted.into();
    let unprivileged = NativeLearning::new(
        nexus.clone(),
        DEFAULT_SPACE.into(),
        AuthContext::system(),
        control,
    )
    .unwrap();
    assert!(
        unprivileged
            .validate_observer(&AuthContext::principal(ungranted), &frozen)
            .await
            .is_err()
    );
    assert!(
        unprivileged
            .execute(&outcome, Some(&AuthContext::principal(ungranted)))
            .await
            .is_err()
    );
    let policy = nexus
        .system_session()
        .read_control(DEFAULT_SPACE, "evaluation_policy/native-policy", None)
        .await
        .unwrap()
        .unwrap();
    let mut value: EvaluationPolicy = serde_json::from_value(policy.value).unwrap();
    value.version = "2".into();
    nexus
        .system_session()
        .set_evaluation_policy(DEFAULT_SPACE, policy.version, value)
        .await
        .unwrap();
    assert!(
        native
            .validate_observer(&AuthContext::principal(OBSERVER), &frozen)
            .await
            .is_err()
    );
    assert!(
        native
            .execute(&outcome, Some(&AuthContext::principal(OBSERVER)))
            .await
            .is_err()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn foreign_rule_binding_cannot_be_silently_accepted() {
    let (db, nexus, native, _, _, _) = setup().await;
    register_paired_rule(&nexus).unwrap();
    assert!(native.restore_rule().is_err());
    db.close().await.unwrap();
}

#[tokio::test]
async fn native_journal_replays_after_database_and_nexus_reopen() {
    let storage = Arc::new(InMemory::new());
    let (db, nexus, native, plan, basis, _) = setup_with_store(storage.clone()).await;
    let frozen = install(&native, &plan).await;
    let intent = attempt_request(
        &plan,
        &basis,
        plan.pairs.keys().next().unwrap(),
        NativeArm::Baseline,
    );
    let committed = native.execute(&intent, None).await.unwrap();
    let persisted = serde_json::to_vec(&intent).unwrap();
    let observer = native.observer.clone();
    db.close().await.unwrap();
    drop(native);
    drop(nexus);
    drop(db);
    let reopened_db = Arc::new(
        AndaDB::connect(storage, crate::testkit::db_config("native_learning"))
            .await
            .unwrap(),
    );
    let reopened = Arc::new(CognitiveNexus::connect(reopened_db.clone()).await.unwrap());
    let native = NativeLearning::new(
        reopened,
        DEFAULT_SPACE.into(),
        AuthContext::system(),
        observer,
    )
    .unwrap();
    native.restore_rule().unwrap();
    let recovered: NativeRequest = serde_json::from_slice(&persisted).unwrap();
    assert_eq!(
        native
            .recover_committed(&recovered, None)
            .await
            .unwrap()
            .unwrap()
            .handles,
        committed.handles
    );
    assert!(native.reconcile(&recovered, None).await.unwrap().is_some());
    assert_eq!(
        native.execute(&recovered, None).await.unwrap().handles,
        committed.handles
    );
    assert_eq!(install(&native, &plan).await, frozen);
    reopened_db.close().await.unwrap();
}

#[tokio::test]
async fn revoked_outcome_permission_is_rechecked_before_writing() {
    let (db, nexus, native, plan, basis, _) = setup().await;
    let frozen = install(&native, &plan).await;
    let observer = AuthContext::principal(OBSERVER);
    native.validate_observer(&observer, &frozen).await.unwrap();
    let pair = plan.pairs.keys().next().unwrap();
    let receipt = native
        .execute(
            &attempt_request(&plan, &basis, pair, NativeArm::Baseline),
            None,
        )
        .await
        .unwrap();
    let request = outcome_request(&plan, &frozen, pair, NativeArm::Baseline, &receipt);
    let grants = nexus
        .governance()
        .grants_for(DEFAULT_SPACE, OBSERVER, &[])
        .await
        .unwrap();
    assert!(!grants.is_empty());
    for grant in grants {
        nexus
            .system_session()
            .revoke_grant(DEFAULT_SPACE, grant._id)
            .await
            .unwrap();
    }
    assert!(native.validate_observer(&observer, &frozen).await.is_err());
    assert!(native.execute(&request, Some(&observer)).await.is_err());
    // Denied history is not converted into "not committed".
    assert!(native.reconcile(&request, Some(&observer)).await.is_err());
    db.close().await.unwrap();
}

#[tokio::test]
async fn committed_outcome_is_read_only_recoverable_after_policy_and_write_revocation() {
    let (db, nexus, native, plan, basis, _) = setup().await;
    let frozen = install(&native, &plan).await;
    let observer = AuthContext::principal(OBSERVER);
    let pair = plan.pairs.keys().next().unwrap();
    let attempt = native
        .execute(
            &attempt_request(&plan, &basis, pair, NativeArm::Baseline),
            None,
        )
        .await
        .unwrap();
    let request = outcome_request(&plan, &frozen, pair, NativeArm::Baseline, &attempt);
    assert!(
        native
            .recover_committed(&request, Some(&observer))
            .await
            .unwrap()
            .is_none()
    );
    let committed = native.execute(&request, Some(&observer)).await.unwrap();
    let policy = nexus
        .system_session()
        .read_control(DEFAULT_SPACE, "evaluation_policy/native-policy", None)
        .await
        .unwrap()
        .unwrap();
    let mut changed_policy: EvaluationPolicy = serde_json::from_value(policy.value).unwrap();
    changed_policy.version = "revoked-for-new-writes".into();
    changed_policy.allowed_parameters.clear();
    nexus
        .system_session()
        .set_evaluation_policy(DEFAULT_SPACE, policy.version, changed_policy)
        .await
        .unwrap();
    for grant in nexus
        .governance()
        .grants_for(DEFAULT_SPACE, OBSERVER, &[])
        .await
        .unwrap()
    {
        nexus
            .system_session()
            .revoke_grant(DEFAULT_SPACE, grant._id)
            .await
            .unwrap();
    }
    nexus
        .governance()
        .create_grant(
            GrantDraft {
                space_id: DEFAULT_SPACE.into(),
                grantee_principal: OBSERVER.into(),
                actions: vec!["read_history".into()],
                ..Default::default()
            },
            SYSTEM_PRINCIPAL,
        )
        .await
        .unwrap();
    assert!(native.validate_observer(&observer, &frozen).await.is_err());
    assert!(native.execute(&request, Some(&observer)).await.is_err());
    let sequence = nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq;
    let recovered = native
        .recover_committed(&request, Some(&observer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.handles, committed.handles);
    assert!(recovered.response.is_none());
    assert_eq!(
        recovered.recovered_transaction.unwrap()["status"],
        "committed"
    );
    assert_eq!(
        nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq,
        sequence
    );
    // Even changing a host-only policy pin that is not repeated in the
    // OutcomeRecord must conflict with the original request parameters digest.
    let mut changed = request.clone();
    if let NativeOperation::Outcome(input) = &mut changed.operation {
        input.frozen.policy_control_version += 1;
    }
    assert_eq!(
        native
            .recover_committed(&changed, Some(&observer))
            .await
            .unwrap_err()
            .code,
        KipErrorCode::IdempotencyConflict
    );
    let mut absent = request.clone();
    absent.idempotency_key.push_str("-never-committed");
    assert!(
        native
            .recover_committed(&absent, Some(&observer))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        native
            .recover_committed(&request, Some(&AuthContext::system()))
            .await
            .is_err()
    );
    for grant in nexus
        .governance()
        .grants_for(DEFAULT_SPACE, OBSERVER, &[])
        .await
        .unwrap()
    {
        nexus
            .system_session()
            .revoke_grant(DEFAULT_SPACE, grant._id)
            .await
            .unwrap();
    }
    assert!(
        native
            .recover_committed(&request, Some(&observer))
            .await
            .is_err()
    );
    db.close().await.unwrap();
}

pub(super) mod dispatch_tests;
