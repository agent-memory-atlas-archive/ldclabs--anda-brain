use super::*;

async fn fixture(
    store: Arc<dyn ObjectStore>,
) -> (
    Arc<crate::space::Space>,
    Arc<LearningRuntime>,
    Arc<crate::runtime::BusinessClock>,
    LearningConfig,
    PairedTrialPlan,
    String,
) {
    let (store, _app, space, mut cfg, mut plan, basis) = setup_store(store).await;
    cfg.maximum_jobs = 8;
    plan.pairs = crate::learning::tests::plan(8).pairs;
    let now = anda_engine::unix_ms();
    plan.execution.cutoff = crate::kip::timestamp(now + 120_000);
    plan.execution.review_due_at = crate::kip::timestamp(now + 240_000);
    let clock = crate::runtime::BusinessClock::manual(now).unwrap();
    let rt = LearningRuntime::connect(
        store,
        "p3_space/learning".into(),
        space.memory.nexus(),
        clock.clone(),
    )
    .await
    .unwrap();
    rt.configure(AuthContext::system(), cfg.clone())
        .await
        .unwrap();
    (space, rt, clock, cfg, plan, basis)
}

async fn cohort(
    rt: &Arc<LearningRuntime>,
    id: &str,
    executor: Arc<Executor>,
    baseline: bool,
    treatment: bool,
) -> JobReport {
    loop {
        match rt.drive(id.into(), executor.clone()).await.unwrap() {
            DriveResult::Dispatched(ticket) => {
                let success = if matches!(ticket.arm, NativeArm::Baseline) {
                    baseline
                } else {
                    treatment
                };
                let submission = outcome(&ticket, success);
                let first = rt.submit_outcome(auth(), submission.clone()).await.unwrap();
                let duplicate = rt.submit_outcome(auth(), submission).await.unwrap();
                assert_eq!(first.outcome_cursor, duplicate.outcome_cursor);
            }
            DriveResult::Ready(report) => return report,
            other => panic!("unexpected cohort state: {other:?}"),
        }
    }
}

async fn skill(nexus: &CognitiveNexus) -> String {
    command(
        nexus,
        "FIND(?s.id) WHERE {?s CONCEPT {type:\"Skill\"}} LIMIT 1",
    )
    .await[0]
        .as_str()
        .unwrap()
        .into()
}

fn context(executor: &Executor, plan: &PairedTrialPlan) -> ApplicationContext {
    let now = anda_engine::unix_ms();
    ApplicationContext {
        executor: executor.identity.clone(),
        revision_ref: plan.candidate_revision.clone(),
        preconditions_satisfied: true,
        evidence_digest: content_digest(&json!("independent precondition instrument fixture"))
            .unwrap(),
        checked_at_ms: now,
        expires_at_ms: now + 30_000,
    }
}

#[tokio::test]
async fn paused_business_clock_does_not_extend_the_real_dispatch_lease() {
    let store = Arc::new(fault_store::CheckpointFault::default());
    let (_, _app, space, mut cfg, mut plan, basis) = setup_store(store.clone()).await;
    cfg.budget.elapsed_ms = 2_000;
    plan.execution.budget = cfg.budget.clone();
    plan.budget_digest = cfg.budget.digest().unwrap();
    cfg.observer.configuration_digest = plan.execution.observer_configuration_digest().unwrap();
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
    rt.enroll("lease".into(), plan.clone(), basis)
        .await
        .unwrap();
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    executor.block.store(true, Ordering::SeqCst);
    store
        .dispatch_checkpoint_delay_ms
        .store(1_200, Ordering::SeqCst);
    let frozen_business_time = clock.now_ms();
    let owned = rt.clone();
    let callback = executor.clone();
    let work = tokio::spawn(async move { owned.drive("lease".into(), callback).await });
    executor.entered.notified().await;
    let ticket = executor.seen.lock()[0].clone();
    let expiry = time_ms(&ticket.started_at).unwrap() + plan.execution.budget.elapsed_ms;
    let remaining = expiry.saturating_sub(anda_engine::unix_ms());
    assert!(
        remaining < 1_000,
        "checkpoint must consume part of the native lease"
    );
    // A frozen business clock would previously allow another full two seconds
    // here, despite the native lease having less than one second remaining.
    let finished =
        tokio::time::timeout(std::time::Duration::from_millis(remaining + 500), work).await;
    if finished.is_err() {
        rt.shutdown().await;
        panic!("external callback exceeded its real native lease");
    }
    assert!(finished.unwrap().unwrap().is_err());
    assert_eq!(clock.now_ms(), frozen_business_time);
    assert_eq!(
        rt.report("lease").await.unwrap().attempts[0].state,
        DispatchState::Reconcile
    );
    rt.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn rejected_review_preflight_and_interrupted_child_creation_preserve_the_obligation() {
    let store = Arc::new(fault_store::CheckpointFault::default());
    let (space, rt, clock, cfg, plan, basis) = fixture(store.clone()).await;
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    rt.enroll("acquire".into(), plan.clone(), basis.clone())
        .await
        .unwrap();
    cohort(&rt, "acquire", executor, false, true).await;
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let adopted = rt.settle("acquire".into()).await.unwrap();
    let mut review = plan.clone();
    review.execution.review_of = Some(adopted.review.unwrap().acquisition);
    for case in review.pairs.values_mut() {
        case.seed = format!("new-review:{}", case.seed);
    }
    review.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 1);
    review.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 120_000);
    clock.advance_to(clock.now_ms() + 2).unwrap();
    let seq = space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    assert!(
        rt.enroll_review(
            "acquire".into(),
            "expired".into(),
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
    review.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 60_000);
    review.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 120_000);
    assert!(
        rt.enroll_review(
            "acquire".into(),
            "bad-basis".into(),
            review.clone(),
            "P-999999".into()
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

    // The source CAS succeeds, then the child's conditional create fails before
    // any native operation. A real restart must recover this exact uncertainty.
    store.fail_enrollment_create.store(true, Ordering::SeqCst);
    assert!(
        rt.enroll_review(
            "acquire".into(),
            "interrupted".into(),
            review.clone(),
            basis.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(
        rt.report("acquire")
            .await
            .unwrap()
            .review
            .unwrap()
            .next_job_id
            .as_deref(),
        Some("interrupted")
    );
    assert!(
        rt.journal
            .read::<Job>(&LearningRuntime::key("interrupted").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    rt.shutdown().await;
    space.close().await.unwrap();
    let db = Arc::new(
        anda_db::database::AndaDB::open(store.clone(), crate::testkit::db_config("p3_space"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    let recovered = LearningRuntime::connect(
        store.clone(),
        "p3_space/learning".into(),
        nexus.clone(),
        clock.clone(),
    )
    .await
    .unwrap();
    clock
        .advance_to(time_ms(&review.execution.cutoff).unwrap())
        .unwrap();
    review.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 60_000);
    review.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 120_000);
    store.fail_missing_job_read.store(true, Ordering::SeqCst);
    assert!(
        recovered
            .enroll_review(
                "acquire".into(),
                "replacement".into(),
                review.clone(),
                basis.clone()
            )
            .await
            .is_err()
    );
    assert_eq!(
        recovered
            .report("acquire")
            .await
            .unwrap()
            .review
            .unwrap()
            .next_job_id
            .as_deref(),
        Some("interrupted"),
        "an uncertain lookup must not reassign the source"
    );
    let (first, second) = tokio::join!(
        recovered.enroll_review(
            "acquire".into(),
            "replacement".into(),
            review.clone(),
            basis.clone()
        ),
        recovered.enroll_review(
            "acquire".into(),
            "competing".into(),
            review.clone(),
            basis.clone()
        )
    );
    assert_ne!(
        first.is_ok(),
        second.is_ok(),
        "only one review may consume the source obligation"
    );
    let child = first.or(second).unwrap();
    assert_eq!(recovered.jobs().await.unwrap().len(), 2);
    assert_eq!(
        recovered
            .report("acquire")
            .await
            .unwrap()
            .review
            .unwrap()
            .next_job_id
            .as_deref(),
        Some(child.job_id.as_str())
    );
    // Once a child checkpoint exists, neither its ID nor frozen parameters may
    // be treated as an absent enrollment and silently replaced.
    assert!(
        recovered
            .enroll_review("acquire".into(), "third".into(), review, basis)
            .await
            .is_err()
    );
    recovered.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn cutoff_adoption_read_gate_new_monitoring_revocation_and_reentry() {
    let (space, rt, clock, cfg, plan, basis) =
        fixture(Arc::new(object_store::memory::InMemory::new())).await;
    let nexus = space.memory.nexus();
    let skill_ref = skill(&nexus).await;
    let executor = Executor::new(&cfg, &plan, nexus.clone());
    rt.enroll("acquire".into(), plan.clone(), basis.clone())
        .await
        .unwrap();
    let ready = cohort(&rt, "acquire", executor.clone(), false, true).await;
    assert!(ready.activation_ref.is_some());
    assert!(
        !rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );
    let seq = nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq;
    assert!(rt.settle("acquire".into()).await.is_err());
    assert_eq!(
        seq,
        nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq,
        "early peek must not write a verdict"
    );
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let adopted = rt.settle("acquire".into()).await.unwrap();
    let acquisition = adopted.review.clone().unwrap().acquisition;
    assert_eq!(adopted.stage, JobStage::Settled);
    let eval = rt
        .record(
            adopted.evaluation_ref.as_deref().unwrap(),
            "EvaluationRecord",
        )
        .await
        .unwrap();
    assert_eq!(eval["comparison"]["status"], "improved");
    assert_eq!(eval["attempt_refs"].as_array().unwrap().len(), 8);
    assert_eq!(
        rt.settle("acquire".into()).await.unwrap().evaluation_ref,
        adopted.evaluation_ref
    );
    rt.bind_application_context(&AuthContext::system(), Some(context(&executor, &plan)))
        .unwrap();
    let seq = nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq;
    let status = rt.procedure_status(&skill_ref).await.unwrap();
    assert!(status.recommendation_allowed, "{status:?}");
    assert!(status.validated_adoption);
    assert_eq!(
        seq,
        nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq,
        "asking whether adopted cannot write learning records"
    );
    let mut drift = context(&executor, &plan);
    drift.executor.environment_digest = content_digest(&json!("changed environment")).unwrap();
    rt.bind_application_context(&AuthContext::system(), Some(drift))
        .unwrap();
    assert!(
        !rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );
    assert!(
        rt.reviews().await.unwrap()[0]
            .reasons
            .contains(&ReviewReason::EnvironmentChanged)
    );
    rt.bind_application_context(&AuthContext::system(), Some(context(&executor, &plan)))
        .unwrap();
    clock
        .advance_to(time_ms(&plan.execution.review_due_at).unwrap())
        .unwrap();
    assert!(
        !rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );
    assert!(rt.reviews().await.unwrap()[0].due);

    let mut abandoned = plan.clone();
    for case in abandoned.pairs.values_mut() {
        case.seed = format!("abandoned:{}", case.seed);
    }
    abandoned.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 60_000);
    abandoned.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 120_000);
    abandoned.execution.review_of = Some(acquisition.clone());
    rt.enroll_review(
        "acquire".into(),
        "abandoned".into(),
        abandoned.clone(),
        basis.clone(),
    )
    .await
    .unwrap();
    clock
        .advance_to(time_ms(&abandoned.execution.cutoff).unwrap())
        .unwrap();
    assert_eq!(
        rt.settle("abandoned".into()).await.unwrap().stage,
        JobStage::Expired
    );
    assert_eq!(
        rt.reviews().await.unwrap()[0].job_id,
        "acquire",
        "a failed baseline cannot consume the persistent review obligation"
    );

    let mut review = plan.clone();
    for case in review.pairs.values_mut() {
        case.seed = format!("monitor:{}", case.seed);
    }
    review.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
    review.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
    review.execution.review_of = Some(acquisition.clone());
    rt.enroll_review(
        "acquire".into(),
        "monitor".into(),
        review.clone(),
        basis.clone(),
    )
    .await
    .unwrap();
    let monitor_executor = Executor::new(&cfg, &review, nexus.clone());
    cohort(&rt, "monitor", monitor_executor, false, false).await;
    clock
        .advance_to(time_ms(&review.execution.cutoff).unwrap())
        .unwrap();
    let revoked = rt.settle("monitor".into()).await.unwrap();
    let verdict = rt
        .record(
            revoked.evaluation_ref.as_deref().unwrap(),
            "EvaluationRecord",
        )
        .await
        .unwrap();
    assert_eq!(verdict["from_status"], "adopted");
    assert_eq!(verdict["to_status"], "revoked");
    assert_eq!(revoked.review.as_ref().unwrap().acquisition, acquisition);
    assert_eq!(
        rt.record(&acquisition.evaluation_ref, "EvaluationRecord")
            .await
            .unwrap(),
        eval,
        "monitoring cannot rewrite acquisition"
    );
    assert!(
        !rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );

    let mut reentry = review.clone();
    for case in reentry.pairs.values_mut() {
        case.seed = format!("reentry:{}", case.seed);
    }
    reentry.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
    reentry.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
    rt.enroll_review("monitor".into(), "reentry".into(), reentry.clone(), basis)
        .await
        .unwrap();
    cohort(
        &rt,
        "reentry",
        Executor::new(&cfg, &reentry, nexus.clone()),
        false,
        true,
    )
    .await;
    clock
        .advance_to(time_ms(&reentry.execution.cutoff).unwrap())
        .unwrap();
    let readopted = rt.settle("reentry".into()).await.unwrap();
    assert_ne!(readopted.trial_ref, adopted.trial_ref);
    assert_ne!(
        readopted.review.unwrap().acquisition.evaluation_ref,
        acquisition.evaluation_ref
    );
    rt.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn incomplete_cohort_and_success_no_better_than_baseline_cannot_adopt() {
    for incomplete in [false, true] {
        let (space, rt, clock, cfg, plan, basis) =
            fixture(Arc::new(object_store::memory::InMemory::new())).await;
        let executor = Executor::new(&cfg, &plan, space.memory.nexus());
        rt.enroll("job".into(), plan.clone(), basis.clone())
            .await
            .unwrap();
        if incomplete {
            for i in 0..9 {
                let DriveResult::Dispatched(ticket) =
                    rt.drive("job".into(), executor.clone()).await.unwrap()
                else {
                    panic!()
                };
                if i < 8 {
                    rt.submit_outcome(auth(), outcome(&ticket, false))
                        .await
                        .unwrap();
                }
            }
        } else {
            cohort(&rt, "job", executor, true, true).await;
        }
        clock
            .advance_to(time_ms(&plan.execution.cutoff).unwrap())
            .unwrap();
        let report = rt.settle("job".into()).await.unwrap();
        let evaluation = rt
            .record(
                report.evaluation_ref.as_deref().unwrap(),
                "EvaluationRecord",
            )
            .await
            .unwrap();
        assert_eq!(evaluation["to_status"], "trialed");
        assert_eq!(
            evaluation["comparison"]["status"],
            if incomplete {
                "insufficient"
            } else {
                "not_improved"
            }
        );
        if incomplete {
            assert_eq!(
                evaluation["missing_attempt_refs"].as_array().unwrap().len(),
                1
            );
        }
        assert!(report.review.is_none());
        if !incomplete {
            let mut fresh = plan.clone();
            for case in fresh.pairs.values_mut() {
                case.seed = format!("new-trial:{}", case.seed);
            }
            fresh.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
            fresh.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
            rt.enroll("fresh".into(), fresh.clone(), basis)
                .await
                .unwrap();
            let executor = Executor::new(&cfg, &fresh, space.memory.nexus());
            for i in 0..9 {
                let DriveResult::Dispatched(ticket) =
                    rt.drive("fresh".into(), executor.clone()).await.unwrap()
                else {
                    panic!()
                };
                if i < 8 {
                    rt.submit_outcome(auth(), outcome(&ticket, false))
                        .await
                        .unwrap();
                }
            }
            let activated = rt.report("fresh").await.unwrap();
            assert!(activated.activation_complete);
            assert_ne!(
                activated.trial_ref, report.trial_ref,
                "closed unproven standing can only be tested through a new Trial"
            );
            clock
                .advance_to(time_ms(&fresh.execution.cutoff).unwrap())
                .unwrap();
            assert_eq!(
                rt.settle("fresh".into()).await.unwrap().stage,
                JobStage::Settled
            );
        }
        rt.shutdown().await;
        space.close().await.unwrap();
    }
}

#[tokio::test]
async fn native_adoption_checkpoint_loss_recovers_once_after_restart() {
    let fault = Arc::new(fault_store::CheckpointFault::default());
    let (space, rt, clock, cfg, plan, basis) = fixture(fault.clone()).await;
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    cohort(&rt, "job", executor, false, true).await;
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    fault.fail_verdict_checkpoint.store(true, Ordering::SeqCst);
    assert!(rt.settle("job".into()).await.is_err());
    assert!(rt.report("job").await.unwrap().evaluation_ref.is_none());
    rt.shutdown().await;
    space.close().await.unwrap();
    let db = Arc::new(
        anda_db::database::AndaDB::open(fault.clone(), crate::testkit::db_config("p3_space"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    let recovered =
        LearningRuntime::connect(fault, "p3_space/learning".into(), nexus.clone(), clock)
            .await
            .unwrap();
    let seq = nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq;
    let receipt = recovered.settle("job".into()).await.unwrap();
    assert_eq!(receipt.stage, JobStage::Settled);
    assert!(receipt.review.is_some());
    assert_eq!(
        seq,
        nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq,
        "native receipt recovery is read-only"
    );
    let skill_ref = skill(&nexus).await;
    assert!(
        !recovered
            .procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed,
        "a restarted host must rebind actual context"
    );
    recovered.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn independent_safety_signal_cancels_inflight_callback_and_revokes_without_quota() {
    let (space, rt, _clock, cfg, plan, basis) =
        fixture(Arc::new(object_store::memory::InMemory::new())).await;
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    let report = rt.enroll("job".into(), plan.clone(), basis).await.unwrap();
    executor.block.store(true, Ordering::SeqCst);
    let entered = executor.entered.notified();
    let running = tokio::spawn({
        let rt = rt.clone();
        let executor = executor.clone();
        async move { rt.drive("job".into(), executor).await }
    });
    entered.await;
    let signal = SafetySubmission {
        space_instance: report.space_instance,
        job_id: "job".into(),
        signal_key: "independent-tool-failure".into(),
        observed_at: anda_cognitive_nexus::time::now(),
        evidence_digest: content_digest(&json!("instrument safety journal")).unwrap(),
        reason: "independent instrument observed unsafe tool behavior".into(),
    };
    assert!(
        rt.submit_safety_signal(AuthContext::system(), signal.clone())
            .await
            .is_err()
    );
    assert!(
        !running.is_finished(),
        "an executor/controller cannot impersonate the observer"
    );
    let revoked = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        rt.submit_safety_signal(auth(), signal.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(running.await.unwrap().is_err());
    assert_eq!(revoked.stage, JobStage::Settled);
    assert_eq!(
        revoked.outcome_cursor, 0,
        "safety is not another task aggregate"
    );
    let safety = revoked.safety.as_ref().unwrap();
    assert!(safety.evidence_ref.is_some() && safety.evaluation_ref.is_some());
    let eval = rt
        .record(
            safety.evaluation_ref.as_deref().unwrap(),
            "EvaluationRecord",
        )
        .await
        .unwrap();
    assert_eq!(eval["comparison"]["status"], "safety_failure");
    assert_eq!(eval["to_status"], "revoked");
    let seq = space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    assert_eq!(
        rt.submit_safety_signal(auth(), signal)
            .await
            .unwrap()
            .evaluation_ref,
        revoked.evaluation_ref
    );
    assert_eq!(
        seq,
        space
            .memory
            .nexus()
            .store
            .get_space(DEFAULT_SPACE)
            .await
            .unwrap()
            .seq
    );
    assert!(matches!(
        rt.drive("job".into(), executor).await.unwrap(),
        DriveResult::Ready(_)
    ));
    rt.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn safety_barrier_closes_the_window_before_dispatch_registers_its_token() {
    let (space, rt, _clock, cfg, plan, basis) =
        fixture(Arc::new(object_store::memory::InMemory::new())).await;
    let executor = Executor::new(&cfg, &plan, space.memory.nexus());
    let report = rt.enroll("job".into(), plan.clone(), basis).await.unwrap();

    // Queue drive first, then let the authenticated safety submission publish
    // its barrier while both operations are waiting for the serialized gate.
    // Whichever waiter resumes first, the callback must not be polled.
    let gate = rt.gate.lock().await;
    let driving = tokio::spawn({
        let rt = rt.clone();
        let executor = executor.clone();
        async move { rt.drive("job".into(), executor).await }
    });
    tokio::task::yield_now().await;
    let signal = SafetySubmission {
        space_instance: report.space_instance,
        job_id: "job".into(),
        signal_key: "pre-dispatch-safety".into(),
        observed_at: anda_cognitive_nexus::time::now(),
        evidence_digest: content_digest(&json!("pre-dispatch safety journal")).unwrap(),
        reason: "independent instrument blocked dispatch before callback".into(),
    };
    let safety = tokio::spawn({
        let rt = rt.clone();
        async move { rt.submit_safety_signal(auth(), signal).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !rt
            .safety_barriers
            .read()
            .values()
            .any(|revision| revision == &plan.candidate_revision)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(gate);

    let _ = driving.await.unwrap();
    assert!(executor.seen.lock().is_empty());
    assert_eq!(safety.await.unwrap().unwrap().stage, JobStage::Settled);
    space.close().await.unwrap();
}
