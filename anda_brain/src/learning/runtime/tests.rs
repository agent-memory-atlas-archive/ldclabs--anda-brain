use super::*;
use anda_brain_fixture::*;
mod r4;
#[cfg(feature = "experiments")]
mod r5;
#[cfg(feature = "experiments")]
mod r5_http;
#[cfg(feature = "experiments")]
mod r6;

// Local fixture module keeps genuine Space/Nexus setup separate from the
// executor instrument. No model, production observer or benchmark oracle runs.
mod anda_brain_fixture {
    use super::*;
    use crate::{
        agents::SELF_USER_ID,
        space::{AppState, Space},
    };
    use anda_cognitive_nexus::governance::store::{GrantDraft, PrincipalDraft};
    use anda_db::database::DBConfig;
    use anda_engine::{
        management::{BaseManagement, Visibility},
        model::Models,
    };
    use object_store::memory::InMemory;
    pub const OBSERVER: &str = "kip:principal:p3-observer";
    pub fn app(store: Arc<dyn ObjectStore>) -> AppState {
        AppState::new(
            store,
            Arc::new(DBConfig {
                name: "p3_runtime".into(),
                description: "fixture".into(),
                storage: Default::default(),
                lock: None,
            }),
            Arc::new(BaseManagement {
                controller: SELF_USER_ID,
                managers: Default::default(),
                visibility: Visibility::Protected,
            }),
            anda_engine::model::reqwest::Client::new(),
            Arc::new(Models::default()),
            Arc::new(vec![]),
            "p3-test".into(),
            "1".into(),
            0,
        )
    }
    pub async fn command(nexus: &CognitiveNexus, cmd: &str) -> Json {
        let r = anda_kip::execute_request(nexus, &Request::single(cmd)).await;
        assert_eq!(r.status, TopLevelStatus::Succeeded, "{r:?}");
        r.first_result().unwrap().clone()
    }
    pub async fn setup() -> (
        Arc<dyn ObjectStore>,
        AppState,
        Arc<Space>,
        LearningConfig,
        PairedTrialPlan,
        String,
    ) {
        setup_store(Arc::new(InMemory::new())).await
    }
    pub async fn setup_store(
        store: Arc<dyn ObjectStore>,
    ) -> (
        Arc<dyn ObjectStore>,
        AppState,
        Arc<Space>,
        LearningConfig,
        PairedTrialPlan,
        String,
    ) {
        setup_store_with_procedure(store, "inspect then prepare if required then commit").await
    }
    pub async fn setup_store_with_procedure(
        store: Arc<dyn ObjectStore>,
        procedure: &str,
    ) -> (
        Arc<dyn ObjectStore>,
        AppState,
        Arc<Space>,
        LearningConfig,
        PairedTrialPlan,
        String,
    ) {
        let app = app(store.clone());
        app.admin_create_space(
            SELF_USER_ID,
            SELF_USER_ID,
            "p3_space".into(),
            7,
            anda_engine::unix_ms(),
        )
        .await
        .unwrap();
        let space = app.load_space_with("p3_space", false, false).await.unwrap();
        let nexus = space.memory.nexus();
        let behavior = json!({"task_family":"tool_workflow.precondition.v1","procedure":procedure});
        let seeded=command(&nexus,&format!(r#"MUTATE {{
            CREATE CONCEPT ?p {{TYPE "Person" NAME "p3 fixture"}}
            CREATE CONCEPT ?v {{TYPE "Preference" NAME "bounded dispatch"}}
            ENSURE PROPOSITION ?fact (?p,"prefers",?v)
            CREATE CONCEPT ?s {{TYPE "Skill" SET ATTRIBUTES {{skill_class:"workflow",summary:"bounded workflow",status:"proposed"}} SET STRUCTURAL {{("current_revision",?r)}}}}
            CREATE CONCEPT ?r {{TYPE "SkillRevision" SET ATTRIBUTES {{task_family:"tool_workflow.precondition.v1",procedure:{},behavior_digest:"{}"}} SET STRUCTURAL {{("revision_of",?s)}}}}
        }}"#,serde_json::to_string(procedure).unwrap(),content_digest(&behavior).unwrap())).await;
        let mut plan = super::super::super::tests::plan(2);
        plan.candidate_revision = seeded["handles"]["r"].as_str().unwrap().into();
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
                    actions: [
                        "discover",
                        "read",
                        "create",
                        "derive",
                        "record_outcome",
                        "read_history",
                    ]
                    .map(str::to_string)
                    .to_vec(),
                    ..Default::default()
                },
                "kip:principal:system",
            )
            .await
            .unwrap();
        crate::learning::native::approve_revision_for_test(
            &nexus,
            &plan.candidate_revision,
            seeded["handles"]["fact"].as_str().unwrap(),
        )
        .await;
        let config = LearningConfig {
            registration_id: "p3-fixture-registration".into(),
            task_family: plan.task_family.clone(),
            controller_control_domain: "fixture-brain-host".into(),
            executor_principal: "kip:principal:p3-business".into(),
            executor_control_domain: "fixture-business-host".into(),
            observer: anda_kip::cognitive::ObserverControl {
                principal_id: OBSERVER.into(),
                configuration_digest: plan.execution.observer_configuration_digest().unwrap(),
                control_domain: "fixture-independent-instrument".into(),
            },
            model_digest: plan.model_digest.clone(),
            tool_versions: plan.tool_versions.clone(),
            budget: plan.execution.budget.clone(),
            alpha: plan.alpha,
            minimum_effect: plan.minimum_effect,
            maximum_failure_rate: plan.execution.maximum_failure_rate,
            minimum_pairs: 2,
            maximum_pairs: 8,
            maximum_jobs: 4,
            reconcile_timeout_ms: 100,
            calibration_digest: content_digest(&json!(
                "protocol-fixture-not-empirical-calibration"
            ))
            .unwrap(),
            rule_digest: content_digest(&super::super::super::paired_rule_artifact()).unwrap(),
        };
        (
            store,
            app,
            space,
            config,
            plan,
            seeded["handles"]["fact"].as_str().unwrap().into(),
        )
    }
    pub fn auth() -> AuthContext {
        let mut a = AuthContext::principal(OBSERVER);
        a.auth_method = "fixture-authenticated-channel".into();
        a
    }
    pub struct Executor {
        pub identity: ExecutorIdentity,
        pub seen: parking_lot::Mutex<Vec<DispatchTicket>>,
        pub lookup: parking_lot::Mutex<ReconcileResult>,
        pub fail: AtomicBool,
        pub block: AtomicBool,
        pub block_lookup: AtomicBool,
        pub entered: tokio::sync::Notify,
        pub nexus: Arc<CognitiveNexus>,
    }
    impl Executor {
        pub fn new(
            config: &LearningConfig,
            plan: &PairedTrialPlan,
            nexus: Arc<CognitiveNexus>,
        ) -> Arc<Self> {
            Arc::new(Self {
                identity: ExecutorIdentity {
                    principal_id: config.executor_principal.clone(),
                    control_domain: config.executor_control_domain.clone(),
                    model_digest: plan.model_digest.clone(),
                    environment_digest: plan.environment_digest.clone(),
                    base_memory_digest: plan.base_memory_digest.clone(),
                    tool_versions: plan.tool_versions.clone(),
                    budget_digest: plan.budget_digest.clone(),
                },
                seen: Default::default(),
                lookup: parking_lot::Mutex::new(ReconcileResult::Unknown),
                fail: AtomicBool::new(false),
                block: AtomicBool::new(false),
                block_lookup: AtomicBool::new(false),
                entered: tokio::sync::Notify::new(),
                nexus,
            })
        }
    }
    impl LearningExecutor for Executor {
        fn identity(&self) -> ExecutorIdentity {
            self.identity.clone()
        }
        fn dispatch(
            &self,
            ticket: DispatchTicket,
            _cancel: CancellationToken,
        ) -> BoxPinFut<Result<(), BoxError>> {
            self.seen.lock().push(ticket.clone());
            let fail = self.fail.load(Ordering::SeqCst);
            let block = self.block.load(Ordering::SeqCst);
            let nexus = self.nexus.clone();
            self.entered.notify_one();
            Box::pin(async move {
                let r = command(
                    &nexus,
                    &format!(
                        "FIND(?a) WHERE {{?a ACTIVITY {{id:{}}}}} LIMIT 1",
                        serde_json::to_string(&ticket.attempt_ref).unwrap()
                    ),
                )
                .await;
                assert_eq!(
                    r.as_array().unwrap().len(),
                    1,
                    "dispatch must see native committed attempt"
                );
                if block {
                    std::future::pending::<()>().await;
                }
                if fail {
                    Err("uncertain dispatch transport error".into())
                } else {
                    Ok(())
                }
            })
        }
        fn reconcile(
            &self,
            _ticket: DispatchTicket,
            _cancel: CancellationToken,
        ) -> BoxPinFut<Result<ReconcileResult, BoxError>> {
            let result = self.lookup.lock().clone();
            let block = self.block_lookup.load(Ordering::SeqCst);
            Box::pin(async move {
                if block {
                    std::future::pending::<()>().await;
                }
                Ok(result)
            })
        }
    }
    pub fn measurements(success: bool) -> OutcomeMeasurements {
        OutcomeMeasurements {
            finished: true,
            accounting_complete: true,
            first_commit_success: Some(success),
            final_committed: Some(success),
            failed_commits: Some(u64::from(!success)),
            unsafe_actions: Some(0),
            tool_calls: Some(1),
            elapsed_ms: Some(2),
            input_tokens: Some(0),
            output_tokens: Some(0),
            journal_digest: content_digest(&json!(["fixture-journal", success])).unwrap(),
        }
    }
    pub fn outcome(t: &DispatchTicket, success: bool) -> OutcomeSubmission {
        OutcomeSubmission {
            space_instance: t.space_instance.clone(),
            job_id: t.job_id.clone(),
            dispatch_id: t.dispatch_id.clone(),
            observer_configuration_digest: t
                .plan
                .execution
                .observer_configuration_digest()
                .unwrap(),
            observation_key: format!("observed:{}", t.dispatch_id),
            observed_at: anda_cognitive_nexus::time::now(),
            measurements: measurements(success),
        }
    }
}

#[tokio::test]
async fn frozen_cohort_dispatches_after_attempt_and_reaches_ready_without_adoption() {
    let (_store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    assert!(!rt.is_configured());
    rt.configure(AuthContext::system(), cfg).await.unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    for i in 0..4 {
        let DriveResult::Dispatched(ticket) =
            rt.drive("job".into(), executor.clone()).await.unwrap()
        else {
            panic!("expected dispatch")
        };
        assert_eq!(matches!(ticket.arm, NativeArm::Baseline), i < 2);
        assert_eq!(ticket.revision.is_none(), i < 2);
        assert!(matches!(
            rt.drive("job".into(), executor.clone()).await.unwrap(),
            DriveResult::Waiting(_)
        ));
        let receipt = outcome(&ticket, i >= 2);
        let first = rt.submit_outcome(auth(), receipt.clone()).await.unwrap();
        let duplicate = rt.submit_outcome(auth(), receipt).await.unwrap();
        assert_eq!(first.outcome_cursor, duplicate.outcome_cursor);
        assert_eq!(first.outcome_cursor, i + 1);
    }
    let DriveResult::Ready(report) = rt.drive("job".into(), executor.clone()).await.unwrap() else {
        panic!("expected ready")
    };
    assert_eq!(report.attempts.len(), 4);
    assert_eq!(executor.seen.lock().len(), 4);
    let status = command(
        &space.memory.nexus(),
        "FIND(?s.attributes.status) WHERE {?s CONCEPT {type:\"Skill\"}} LIMIT 10",
    )
    .await;
    assert_eq!(status, json!(["trialed"]));
    space.close().await.unwrap();
}

#[tokio::test]
async fn restart_restores_rule_and_reconciles_uncertain_dispatch_without_resending() {
    let (store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let old = Executor::new(&cfg, &plan, space.memory.nexus());
    old.fail.store(true, Ordering::SeqCst);
    assert!(rt.drive("job".into(), old.clone()).await.is_err());
    assert_eq!(old.seen.lock().len(), 1);
    let ticket = old.seen.lock()[0].clone();
    space.close().await.unwrap();
    let reopened = app(store)
        .load_space_with("p3_space", false, false)
        .await
        .unwrap();
    let rt = reopened.learning();
    assert!(rt.is_configured());
    assert_eq!(
        rt.jobs()
            .await
            .unwrap()
            .iter()
            .map(|j| j.job_id.as_str())
            .collect::<Vec<_>>(),
        vec!["job"]
    );
    let executor = Executor::new(&cfg, &plan, reopened.memory.nexus());
    assert!(matches!(
        rt.drive("job".into(), executor.clone()).await.unwrap(),
        DriveResult::Reconciled(_, ReconcileResult::Unknown)
    ));
    assert!(executor.seen.lock().is_empty());
    *executor.lookup.lock() = ReconcileResult::Finished;
    assert!(matches!(
        rt.drive("job".into(), executor.clone()).await.unwrap(),
        DriveResult::Reconciled(_, ReconcileResult::Finished)
    ));
    rt.submit_outcome(auth(), outcome(&ticket, false))
        .await
        .unwrap();
    assert_eq!(rt.report("job").await.unwrap().outcome_cursor, 1);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn forged_cross_space_early_conflicting_and_unknown_outcomes_cannot_add_success() {
    let (_store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    let DriveResult::Dispatched(t) = rt.drive("job".into(), executor).await.unwrap() else {
        panic!()
    };
    let good = outcome(&t, true);
    let mut progress = good.clone();
    progress.measurements.finished = false;
    assert!(rt.submit_outcome(auth(), progress).await.is_err());
    assert_eq!(rt.report("job").await.unwrap().outcome_cursor, 0);
    assert!(
        rt.submit_outcome(AuthContext::system(), good.clone())
            .await
            .is_err()
    );
    let mut other = good.clone();
    other.space_instance = content_digest(&json!("other-space")).unwrap();
    assert!(rt.submit_outcome(auth(), other).await.is_err());
    let mut future = good.clone();
    future.observed_at = "2098-01-01T00:00:00.000Z".into();
    assert!(rt.submit_outcome(auth(), future).await.is_err());
    let mut early = good.clone();
    early.observed_at = "2000-01-01T00:00:00.000Z".into();
    assert!(rt.submit_outcome(auth(), early).await.is_err());
    let mut unknown = good.clone();
    unknown.measurements.accounting_complete = false;
    let reported = rt.submit_outcome(auth(), unknown).await.unwrap();
    assert_eq!(reported.outcome_cursor, 1);
    assert!(rt.submit_outcome(auth(), good).await.is_err());
    assert_eq!(rt.report("job").await.unwrap().outcome_cursor, 1);
    let reference = reported.attempts[0].outcome_ref.as_ref().unwrap();
    let rows = command(
        &space.memory.nexus(),
        &format!(
            "FIND(?e) WHERE {{?e EVIDENCE {{id:{}}}}} LIMIT 1",
            serde_json::to_string(reference).unwrap()
        ),
    )
    .await;
    assert_eq!(
        rows[0]["facets"]["kip://profiles/cognitive-memory@2.1.0/OutcomeRecord"]["outcome_status"],
        "unknown"
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn close_cancels_owned_executor_and_persists_reconcile_state() {
    let (_store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    executor.block.store(true, Ordering::SeqCst);
    let entered = executor.entered.notified();
    let r = rt.clone();
    let e = executor.clone();
    let waiter = tokio::spawn(async move { r.drive("job".into(), e).await });
    entered.await;
    assert!(space.is_processing());
    space.close().await.unwrap();
    assert!(waiter.await.unwrap().is_err());
    assert!(rt.drive("job".into(), executor).await.is_err());
}

#[tokio::test]
async fn disabled_runtime_and_changed_identity_do_not_dispatch() {
    let (_store, app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    rt.set_enabled(AuthContext::system(), false).await.unwrap();
    assert!(rt.drive("job".into(), executor.clone()).await.is_err());
    assert!(executor.seen.lock().is_empty());
    rt.set_enabled(AuthContext::system(), true).await.unwrap();
    let mut wrong = executor.identity.clone();
    wrong.base_memory_digest = content_digest(&json!("different snapshot")).unwrap();
    assert!(cfg.validate_executor(&plan, &wrong).is_err());
    assert!(app.fork_space("p3_space", None).await.is_err());
    space.close().await.unwrap();
}

mod fault_store;
#[cfg(feature = "experiments")]
mod p4;
#[cfg(feature = "experiments")]
mod p6;

#[tokio::test]
#[cfg(feature = "experiments")]
async fn committed_outcome_recovers_after_checkpoint_failure_restart_and_cutoff() {
    let fault = Arc::new(fault_store::CheckpointFault::default());
    let (store, _app, space, cfg, mut plan, basis) = setup_store(fault.clone()).await;
    let now = anda_engine::unix_ms();
    plan.execution.cutoff = crate::kip::timestamp(now + 60_000);
    plan.execution.review_due_at = crate::kip::timestamp(now + 120_000);
    // One active coordinator; the Space's default (unconfigured) coordinator
    // stays idle. A manual host clock makes the crash/cutoff test deterministic.
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
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    let DriveResult::Dispatched(ticket) = rt.drive("job".into(), executor).await.unwrap() else {
        panic!()
    };
    let observed = outcome(&ticket, false);
    fault.fail_checkpoint.store(true, Ordering::SeqCst);
    assert!(rt.submit_outcome(auth(), observed.clone()).await.is_err());
    let report = rt.report("job").await.unwrap();
    assert_eq!(report.outcome_cursor, 0);
    assert!(
        rt.journal
            .read::<Job>(&LearningRuntime::key("job").unwrap())
            .await
            .unwrap()
            .unwrap()
            .value
            .pending
            .is_some()
    );
    rt.shutdown().await;
    space.close().await.unwrap();
    let db = Arc::new(
        anda_db::database::AndaDB::open(store.clone(), crate::testkit::db_config("p3_space"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    clock.advance_to(now + 61_000).unwrap();
    let recovered =
        LearningRuntime::connect(store, "p3_space/learning".into(), nexus.clone(), clock)
            .await
            .unwrap();
    let result = recovered
        .submit_outcome(auth(), observed.clone())
        .await
        .unwrap();
    assert_eq!(result.outcome_cursor, 1);
    assert_eq!(result.attempts[0].state, DispatchState::Observed);
    assert_eq!(
        recovered
            .submit_outcome(auth(), observed)
            .await
            .unwrap()
            .outcome_cursor,
        1
    );
    let count = command(
        &nexus,
        r#"FIND(?e.id) WHERE {?e EVIDENCE {evidence_class:"outcome"}} LIMIT 10"#,
    )
    .await;
    assert_eq!(
        count.as_array().unwrap().len(),
        1,
        "recovery must not create another native outcome"
    );
    recovered.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn durable_journal_ack_loss_is_read_back_before_any_external_dispatch() {
    let fault = Arc::new(fault_store::CheckpointFault::default());
    let (_store, _app, space, cfg, plan, basis) = setup_store(fault.clone()).await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    fault.lose_dispatch_ack.store(true, Ordering::SeqCst);
    assert!(matches!(
        rt.drive("job".into(), executor.clone()).await.unwrap(),
        DriveResult::Dispatched(_)
    ));
    assert_eq!(executor.seen.lock().len(), 1);
    assert!(matches!(
        rt.drive("job".into(), executor.clone()).await.unwrap(),
        DriveResult::Waiting(_)
    ));
    assert_eq!(executor.seen.lock().len(), 1);
    space.close().await.unwrap();
}

#[tokio::test]
async fn hanging_lookup_is_bounded_and_never_becomes_not_started() {
    let (_store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    executor.fail.store(true, Ordering::SeqCst);
    assert!(rt.drive("job".into(), executor.clone()).await.is_err());
    executor.block_lookup.store(true, Ordering::SeqCst);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        rt.drive("job".into(), executor.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        result,
        DriveResult::Reconciled(_, ReconcileResult::Unknown)
    ));
    assert_eq!(executor.seen.lock().len(), 1);
    space.close().await.unwrap();
}

#[tokio::test]
async fn changed_policy_prevents_new_dispatch() {
    let (_store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let reg = rt.registration(false).await.unwrap();
    let job = rt.load_job(&reg, "job").await.unwrap().value;
    let pin = job.frozen.unwrap();
    let session = space.memory.nexus().session(AuthContext::system());
    let old = session
        .read_control(
            DEFAULT_SPACE,
            &format!("evaluation_policy/{}", pin.evaluation_policy.id),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let mut policy: anda_kip::cognitive::EvaluationPolicy =
        serde_json::from_value(old.value).unwrap();
    policy.version = "2".into();
    session
        .set_evaluation_policy(DEFAULT_SPACE, old.version, policy)
        .await
        .unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    assert!(rt.drive("job".into(), executor.clone()).await.is_err());
    assert!(executor.seen.lock().is_empty());
    space.close().await.unwrap();
}

#[tokio::test]
async fn authoritative_not_started_still_cannot_bypass_native_dispatch_state() {
    let (_store, _app, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    executor.fail.store(true, Ordering::SeqCst);
    assert!(rt.drive("job".into(), executor.clone()).await.is_err());
    *executor.lookup.lock() = ReconcileResult::NotStarted;
    executor.fail.store(false, Ordering::SeqCst);
    for _ in 0..2 {
        assert!(matches!(
            rt.drive("job".into(), executor.clone()).await.unwrap(),
            DriveResult::Reconciled(_, ReconcileResult::NotStarted)
        ));
    }
    assert_eq!(executor.seen.lock().len(), 1);
    space.close().await.unwrap();
}

#[test]
fn complete_counter_fields_do_not_imply_complete_accounting() {
    let plan = super::super::tests::plan(2);
    let mut measured = measurements(true);
    assert_eq!(
        measured.classify(&plan).unwrap(),
        NativeOutcomeStatus::Success
    );
    measured.accounting_complete = false;
    assert_eq!(
        measured.classify(&plan).unwrap(),
        NativeOutcomeStatus::Unknown
    );
    measured.accounting_complete = true;
    measured.input_tokens = None;
    assert_eq!(
        measured.classify(&plan).unwrap(),
        NativeOutcomeStatus::Unknown
    );
}
