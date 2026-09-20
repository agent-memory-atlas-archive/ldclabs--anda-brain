use super::*;
use crate::learning::*;

struct AutomaticExecutor(Arc<Executor>);
impl LearningExecutor for AutomaticExecutor {
    fn identity(&self) -> ExecutorIdentity {
        self.0.identity()
    }
    fn preflight(&self, _: CancellationToken) -> BoxPinFut<Result<(), BoxError>> {
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&self, t: DispatchTicket, c: CancellationToken) -> BoxPinFut<Result<(), BoxError>> {
        self.0.dispatch(t, c)
    }
    fn reconcile(
        &self,
        t: DispatchTicket,
        c: CancellationToken,
    ) -> BoxPinFut<Result<ReconcileResult, BoxError>> {
        self.0.reconcile(t, c)
    }
}
struct Instrument(anda_kip::cognitive::ObserverControl);
#[async_trait::async_trait]
impl LearningObserver for Instrument {
    fn control(&self) -> anda_kip::cognitive::ObserverControl {
        self.0.clone()
    }
    async fn authenticate(&self, _: CancellationToken) -> Result<AuthContext, BoxError> {
        Ok(auth())
    }
    async fn observe(
        &self,
        t: &DispatchTicket,
        _: CancellationToken,
    ) -> Result<Option<LearningObservation>, BoxError> {
        let success = !matches!(t.arm, NativeArm::Baseline);
        let replay = json!({"mechanism_fixture":true,"dispatch":t.dispatch_id,"success":success});
        let mut submission = outcome(t, success);
        submission.measurements.journal_digest = content_digest(&replay)?;
        Ok(Some(LearningObservation {
            input: r4::input(t, &submission),
            replay,
        }))
    }
}
struct Factory {
    source: (String, String),
    first: LearningEnrollment,
    reviews: parking_lot::Mutex<BTreeMap<String, LearningEnrollment>>,
}
#[async_trait::async_trait]
impl LearningPlanFactory for Factory {
    fn source(&self) -> (String, String) {
        self.source.clone()
    }
    async fn next(
        &self,
        _: &str,
        after: Option<&str>,
        review: Option<&ReviewStatus>,
        _: CancellationToken,
    ) -> Result<Option<LearningEnrollment>, BoxError> {
        Ok(if let Some(review) = review {
            self.reviews.lock().get(&review.job_id).cloned()
        } else if after == Some(self.first.origin.event_key.as_str()) {
            None
        } else {
            Some(self.first.clone())
        })
    }
}

#[tokio::test]
async fn r5_scheduler_retains_missing_cohort_then_enrolls_review_and_consumes_late_safety() {
    let (store, _, space, mut cfg, mut plan, basis) = setup().await;
    cfg.maximum_jobs = 1;
    plan.pairs = crate::learning::tests::plan(8).pairs;
    let clock = crate::runtime::BusinessClock::manual(anda_engine::unix_ms()).unwrap();
    plan.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
    plan.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
    let calibration = json!(LearningCalibration {
        format: "anda-brain:learning-calibration-v1".into(),
        reviewed_by: "kip:principal:fixture-operator".into(),
        approved_for_automatic_trials: true,
        contract_digest: calibration_contract(&cfg).unwrap(),
        environment_digest: plan.environment_digest.clone(),
        training_manifest: json!({"fixture":"train"}),
        validation_manifest: json!({"fixture":"validation"}),
        report: json!({"fixture":true})
    });
    cfg.calibration_digest = content_digest(&calibration).unwrap();
    let rt = LearningRuntime::connect(
        store.clone(),
        "p3_space/learning".into(),
        space.memory.nexus(),
        clock.clone(),
    )
    .await
    .unwrap();
    rt.bind_attention(Arc::downgrade(&space.attention()));
    let source: (String, String) = (
        "predeclared-fixture-source".into(),
        content_digest(&json!("registered source contract")).unwrap(),
    );
    let origin = EnrollmentOrigin {
        source_id: source.0.clone(),
        source_digest: source.1.clone(),
        event_key: "acquire".into(),
        trigger_ref: None,
        gate_wake_ref: None,
    };
    let factory = Arc::new(Factory {
        source,
        first: LearningEnrollment {
            job_id: "auto-acquire".into(),
            plan: plan.clone(),
            basis_proposition: basis.clone(),
            origin: origin.clone(),
        },
        reviews: Default::default(),
    });
    let executor = Arc::new(AutomaticExecutor(Executor::new(
        &cfg,
        &plan,
        space.memory.nexus(),
    )));
    rt.install_bindings(
        AuthContext::system(),
        LearningBindings {
            registration: cfg.clone(),
            automation: LearningAutomation {
                trials: true,
                reviews: true,
                archive: true,
                safety: true,
            },
            storage: LearningStoragePolicy::default(),
            executor,
            observer: Arc::new(Instrument(cfg.observer.clone())),
            plans: factory.clone(),
            calibration,
            callback_timeout_ms: 10_000,
        },
    )
    .await
    .unwrap();
    let ingress = r4::ingress(&space, store, &rt).await;
    assert_eq!(
        rt.automatic_pass(Some(ingress.clone()))
            .await
            .unwrap()
            .enrolled,
        1
    );
    for _ in 0..17 {
        let pass = rt.automatic_pass(Some(ingress.clone())).await.unwrap();
        assert!(pass.blocked.is_empty(), "{pass:?}");
    }
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let pass = rt.automatic_pass(Some(ingress.clone())).await.unwrap();
    assert_eq!(pass.archived, 1, "{pass:?}");
    let acquired = rt.report("auto-acquire").await.unwrap();
    for hint in rt.attention_reviews().await.unwrap() {
        space.attention().schedule_recheck(hint).await.unwrap();
    }
    clock
        .advance_to(time_ms(&plan.execution.review_due_at).unwrap())
        .unwrap();
    let unavailable = rt.automatic_pass(Some(ingress.clone())).await.unwrap();
    assert!(
        unavailable
            .blocked
            .iter()
            .any(|s| s == "review_due_without_fresh_authorized_cohort")
    );
    assert!(
        rt.report("auto-acquire")
            .await
            .unwrap()
            .review
            .unwrap()
            .next_job_id
            .is_none()
    );
    assert!(
        space
            .attention()
            .status()
            .await
            .unwrap()
            .unwrap()
            .rechecks
            .iter()
            .any(|r| r.key == "skill-review:auto-acquire")
    );
    let mut review = plan.clone();
    review.execution.review_of = Some(acquired.review.clone().unwrap().acquisition);
    review.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
    review.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
    for case in review.pairs.values_mut() {
        case.seed = format!("monitor:{}", case.seed);
    }
    factory.reviews.lock().insert(
        "auto-acquire".into(),
        LearningEnrollment {
            job_id: "auto-review".into(),
            plan: review,
            basis_proposition: basis,
            origin: EnrollmentOrigin {
                event_key: "fresh-monitoring".into(),
                ..origin
            },
        },
    );
    assert_eq!(
        rt.automatic_pass(Some(ingress.clone()))
            .await
            .unwrap()
            .reviews_enrolled,
        1
    );
    rt.attention_reviews().await.unwrap();
    assert!(
        !space
            .attention()
            .status()
            .await
            .unwrap()
            .unwrap()
            .rechecks
            .iter()
            .any(|r| r.key == "skill-review:auto-acquire")
    );
    let reg = rt.registration(false).await.unwrap();
    let job = rt.load_job(&reg, "auto-acquire").await.unwrap().value;
    let a = job.attempts.values().next().unwrap();
    let ticket = rt.hydrate_ticket(&job, a).await.unwrap();
    let mut input = r4::input(&ticket, &outcome(&ticket, false));
    input.event_key = "late-independent-safety".into();
    input.safety_signal = Some("independent hazardous effect discovered after cutoff".into());
    let receipt = ingress.submit(auth(), input.clone()).await.unwrap();
    assert!(receipt.safety_pending && !receipt.learning_eligible);
    // Receipt index includes accepted/committed snapshots, so several bounded
    // pages may precede this later signal. No snapshot is skipped as complete.
    let mut resolved = 0;
    for _ in 0..6 {
        resolved += ingress.consume_learning_safety(auth(), 8).await.unwrap();
    }
    assert_eq!(resolved, 1);
    let revoked = rt.report("auto-acquire").await.unwrap();
    assert!(revoked.safety.unwrap().evaluation_ref.is_some());
    assert_eq!(
        rt.archive_replay("auto-acquire")
            .await
            .unwrap()
            .evaluation_ref,
        acquired.evaluation_ref
    );
    assert_ne!(revoked.evaluation_ref, acquired.evaluation_ref);
    // Another authenticated signal stays auditable but may be covered by the
    // exact still-current revocation. It must not stall all later receipts.
    input.event_key = "second-late-independent-safety".into();
    let repeated = ingress.submit(auth(), input).await.unwrap();
    assert!(repeated.safety_pending);
    let seq = space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let mut covered = 0;
    for _ in 0..3 {
        covered += ingress.consume_learning_safety(auth(), 8).await.unwrap();
    }
    assert_eq!(covered, 1);
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
    ingress.shutdown().await;
    rt.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn r5_more_than_32_native_terminal_jobs_release_hot_capacity_and_replay_after_restart() {
    let (store, _, space, mut cfg, mut plan, basis) = setup().await;
    cfg.maximum_jobs = 1;
    let clock = crate::runtime::BusinessClock::manual(anda_engine::unix_ms()).unwrap();
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
    let mut first = None;
    for n in 0..34 {
        plan.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
        plan.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
        for case in plan.pairs.values_mut() {
            case.seed = format!("retained-{n}:{}", case.task_digest);
        }
        let id = format!("retained-{n}");
        rt.enroll(id.clone(), plan.clone(), basis.clone())
            .await
            .unwrap();
        p4::cohort(
            &rt,
            &id,
            Executor::new(&cfg, &plan, space.memory.nexus()),
            false,
            false,
        )
        .await;
        clock
            .advance_to(time_ms(&plan.execution.cutoff).unwrap())
            .unwrap();
        let settled = rt.settle(id.clone()).await.unwrap();
        assert_eq!(settled.stage, JobStage::Settled);
        let archived = rt.archive(id.clone()).await.unwrap();
        assert!(archived.archive.is_some());
        assert!(rt.jobs().await.unwrap().is_empty());
        if n == 0 {
            first = Some((plan.clone(), archived));
        }
    }
    let capacity = rt.capacity().await.unwrap();
    assert_eq!(capacity.archived_jobs, 34);
    assert_eq!(capacity.retained_identities, 34);
    assert_eq!(capacity.hot_jobs, 0);
    let first_page = rt.jobs_page(0, 32).await.unwrap();
    assert_eq!(first_page.items.len(), 32);
    assert!(!first_page.complete);
    assert_eq!(
        rt.jobs_page(first_page.next_after, 32)
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    assert!(
        rt.reviews().await.is_err(),
        "a bounded legacy call must not report the first 32 as complete"
    );
    let (old_plan, old) = first.unwrap();
    assert!(
        rt.enroll(old.job_id.clone(), plan.clone(), basis.clone())
            .await
            .is_err()
    );
    rt.shutdown().await;
    space.close().await.unwrap();
    let db = Arc::new(
        anda_db::database::AndaDB::open(store.clone(), crate::testkit::db_config("p3_space"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    let restored =
        LearningRuntime::connect(store, "p3_space/learning".into(), nexus.clone(), clock)
            .await
            .unwrap();
    let seq = nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq;
    assert_eq!(
        restored.report(&old.job_id).await.unwrap().evaluation_ref,
        old.evaluation_ref
    );
    assert_eq!(
        restored
            .settle(old.job_id.clone())
            .await
            .unwrap()
            .evaluation_ref,
        old.evaluation_ref
    );
    assert_eq!(
        restored
            .enroll(old.job_id.clone(), old_plan, basis)
            .await
            .unwrap()
            .evaluation_ref,
        old.evaluation_ref
    );
    assert_eq!(
        nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq,
        seq,
        "history lookup/replay creates no new native verdict"
    );
    restored.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn r5_archived_acquisition_keeps_applicability_review_capacity_and_safety_revocation() {
    let (space, rt, clock, cfg, plan, basis) =
        p4::fixture(Arc::new(object_store::memory::InMemory::new())).await;
    rt.enroll("acquire".into(), plan.clone(), basis.clone())
        .await
        .unwrap();
    p4::cohort(
        &rt,
        "acquire",
        Executor::new(&cfg, &plan, space.memory.nexus()),
        false,
        true,
    )
    .await;
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let acquired = rt.settle("acquire".into()).await.unwrap();
    let original = acquired.evaluation_ref.clone();
    rt.archive("acquire".into()).await.unwrap();
    assert!(
        rt.procedure_status(&p4::skill(&space.memory.nexus()).await)
            .await
            .unwrap()
            .validated_adoption
    );
    assert_eq!(rt.reviews_page(0, 8).await.unwrap().items.len(), 1);
    let mut review = plan.clone();
    review.execution.review_of = Some(acquired.review.unwrap().acquisition);
    review.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
    review.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
    for case in review.pairs.values_mut() {
        case.seed = format!("new:{}", case.seed);
    }
    rt.configure_storage(
        AuthContext::system(),
        LearningStoragePolicy {
            maximum_records: 1,
            maximum_reserved_bytes: 64 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert!(
        rt.enroll_review(
            "acquire".into(),
            "review".into(),
            review.clone(),
            basis.clone()
        )
        .await
        .is_err()
    );
    assert!(
        rt.report("acquire")
            .await
            .unwrap()
            .review
            .unwrap()
            .next_job_id
            .is_none()
    );
    rt.configure_storage(AuthContext::system(), LearningStoragePolicy::default())
        .await
        .unwrap();
    let reviewed = rt
        .enroll_review("acquire".into(), "review".into(), review, basis)
        .await
        .unwrap();
    assert_eq!(reviewed.stage, JobStage::Baseline);
    let revoked = rt
        .submit_safety_signal(
            auth(),
            SafetySubmission {
                space_instance: acquired.space_instance,
                job_id: "acquire".into(),
                signal_key: "after-archive".into(),
                observed_at: anda_cognitive_nexus::time::now(),
                evidence_digest: content_digest(&json!(
                    "independent safety observation after archive"
                ))
                .unwrap(),
                reason: "independent unsafe event".into(),
            },
        )
        .await
        .unwrap();
    assert!(revoked.safety.unwrap().evaluation_ref.is_some());
    assert_ne!(revoked.evaluation_ref, original);
    assert_eq!(
        rt.archive_replay("acquire").await.unwrap().evaluation_ref,
        original
    );
    assert!(
        !rt.procedure_status(&p4::skill(&space.memory.nexus()).await)
            .await
            .unwrap()
            .recommendation_allowed
    );
    assert_eq!(rt.capacity().await.unwrap().archived_jobs, 1);
    assert!(rt.report("acquire").await.unwrap().archive.is_some());
    rt.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn r5_archive_snapshot_ack_and_hot_release_recover_without_losing_old_verdict() {
    let store = Arc::new(fault_store::CheckpointFault::default());
    let (space, rt, clock, cfg, plan, basis) = p4::fixture(store.clone()).await;
    rt.enroll("archive-fault".into(), plan.clone(), basis)
        .await
        .unwrap();
    p4::cohort(
        &rt,
        "archive-fault",
        Executor::new(&cfg, &plan, space.memory.nexus()),
        false,
        true,
    )
    .await;
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let verdict = rt.settle("archive-fault".into()).await.unwrap();
    store.lose_archive_ack.store(true, Ordering::SeqCst);
    store.fail_archive_checkpoint.store(true, Ordering::SeqCst);
    assert!(rt.archive("archive-fault".into()).await.is_err());
    assert_eq!(rt.capacity().await.unwrap().hot_jobs, 1);
    // A safety update between archive checkpoint retries must not overwrite or
    // strand the original immutable archive snapshot.
    rt.submit_safety_signal(
        auth(),
        SafetySubmission {
            space_instance: verdict.space_instance,
            job_id: "archive-fault".into(),
            signal_key: "archive-race".into(),
            observed_at: anda_cognitive_nexus::time::now(),
            evidence_digest: content_digest(&json!("archive safety receipt")).unwrap(),
            reason: "independent safety report".into(),
        },
    )
    .await
    .unwrap();
    store.fail_archive_catalog.store(true, Ordering::SeqCst);
    assert!(rt.archive("archive-fault".into()).await.is_err());
    assert!(rt.report("archive-fault").await.unwrap().archive.is_some());
    rt.shutdown().await;
    space.close().await.unwrap();
    store.forbid_learning_list.store(true, Ordering::SeqCst);
    let db = Arc::new(
        anda_db::database::AndaDB::open(store.clone(), crate::testkit::db_config("p3_space"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    let recovered = LearningRuntime::connect(store, "p3_space/learning".into(), nexus, clock)
        .await
        .unwrap();
    assert_eq!(recovered.capacity().await.unwrap().hot_jobs, 0);
    assert_eq!(recovered.capacity().await.unwrap().archived_jobs, 1);
    assert_eq!(
        recovered
            .archive_replay("archive-fault")
            .await
            .unwrap()
            .evaluation_ref,
        verdict.evaluation_ref
    );
    assert!(
        recovered
            .report("archive-fault")
            .await
            .unwrap()
            .safety
            .unwrap()
            .evaluation_ref
            .is_some()
    );
    recovered.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn r5_v1_journals_migrate_once_then_discover_by_numeric_catalog_without_listing() {
    use futures::TryStreamExt;
    let store = Arc::new(fault_store::CheckpointFault::default());
    let (_, _, space, cfg, plan, basis) = setup_store(store.clone()).await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg).await.unwrap();
    let enrolled = rt.enroll("legacy".into(), plan, basis).await.unwrap();
    space.close().await.unwrap();
    // Simulate an exact v1 journal store: only jobs and registration existed.
    for prefix in ["p3_space/learning/catalog", "p3_space/learning/enrollments"] {
        let objects = store
            .list(Some(&object_store::path::Path::from(prefix)))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for object in objects {
            object_store::ObjectStoreExt::delete(store.as_ref(), &object.location)
                .await
                .unwrap();
        }
    }
    let db = Arc::new(
        anda_db::database::AndaDB::open(store.clone(), crate::testkit::db_config("p3_space"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    let recovered = LearningRuntime::connect(
        store.clone(),
        "p3_space/learning".into(),
        nexus,
        Arc::new(crate::runtime::BusinessClock::default()),
    )
    .await
    .unwrap();
    store.forbid_learning_list.store(true, Ordering::SeqCst);
    assert_eq!(
        recovered.jobs().await.unwrap()[0].space_instance,
        enrolled.space_instance
    );
    assert_eq!(
        recovered.jobs_page(0, 1).await.unwrap().items[0].job_id,
        "legacy"
    );
    assert_eq!(recovered.capacity().await.unwrap().retained_identities, 1);
    recovered.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn r5_independent_terminal_receipt_can_finish_bookkeeping_after_lease_expiry() {
    let (_, _, space, cfg, plan, basis) = setup().await;
    let rt = space.learning();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    rt.enroll("lease-receipt".into(), plan.clone(), basis)
        .await
        .unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    let DriveResult::Dispatched(ticket) = rt
        .drive("lease-receipt".into(), executor.clone())
        .await
        .unwrap()
    else {
        panic!("missing dispatch")
    };
    let expiry = time_ms(&ticket.started_at).unwrap() + plan.execution.budget.elapsed_ms;
    tokio::time::sleep(std::time::Duration::from_millis(
        expiry.saturating_sub(anda_engine::unix_ms()) + 10,
    ))
    .await;
    let observed = rt
        .submit_outcome(auth(), outcome(&ticket, false))
        .await
        .unwrap();
    assert!(observed.attempts[0].task_completed && observed.attempts[0].dispatch_reconciled);
    assert_eq!(executor.seen.lock().len(), 1);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r5_unknown_dispatch_and_missing_outcomes_are_never_archived_as_success() {
    let (space, rt, clock, cfg, plan, basis) =
        p4::fixture(Arc::new(object_store::memory::InMemory::new())).await;
    rt.enroll("unknown".into(), plan.clone(), basis)
        .await
        .unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    executor.fail.store(true, Ordering::SeqCst);
    assert!(rt.drive("unknown".into(), executor).await.is_err());
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    assert_eq!(
        rt.settle("unknown".into()).await.unwrap().stage,
        JobStage::Expired
    );
    assert!(rt.archive("unknown".into()).await.is_err());
    assert_eq!(rt.capacity().await.unwrap().hot_jobs, 1);
    assert_eq!(rt.report("unknown").await.unwrap().evaluation_ref, None);
    rt.shutdown().await;
    space.close().await.unwrap();
}
