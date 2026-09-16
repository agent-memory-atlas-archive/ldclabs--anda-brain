//! Longitudinal mechanism tests against the actual native record/dispatch
//! pipeline. The business policy is deterministic, with a separate receipt
//! verifier; none of these results are empirical model-learning evidence.
use super::*;
use crate::learning::workflow_fixture::{self as world, Policy, Requirement};

const WRONG: &str = "always prepare then commit";

struct InstrumentedExecutor {
    native: Arc<Executor>,
    cases: BTreeMap<String, Requirement>,
    receipts: Arc<parking_lot::Mutex<BTreeMap<String, Json>>>,
}

impl LearningExecutor for InstrumentedExecutor {
    fn identity(&self) -> ExecutorIdentity {
        self.native.identity()
    }
    fn dispatch(
        &self,
        ticket: DispatchTicket,
        cancel: CancellationToken,
    ) -> BoxPinFut<Result<(), BoxError>> {
        let native = self.native.clone();
        let requirement = self.cases[&ticket.pair_id];
        let receipts = self.receipts.clone();
        Box::pin(async move {
            // Checks the committed native Attempt before any tool action.
            native.dispatch(ticket.clone(), cancel).await?;
            let policy = if let Some(revision) = &ticket.revision {
                // Exactly the immutable program authorized in this test's
                // setup, never substitute a better/worse procedure post hoc.
                assert_eq!(revision["attributes"]["procedure"], WRONG);
                Policy::PrepareThenCommit
            } else {
                Policy::Commit
            };
            let receipt = world::execute(requirement, policy, &ticket.plan.execution.budget);
            let mut verified = world::verify(&receipt, &ticket.plan.execution.budget);
            verified["journal_digest"] = content_digest(&json!(receipt)).unwrap().into();
            receipts.lock().insert(ticket.dispatch_id, verified);
            Ok(())
        })
    }
    fn reconcile(
        &self,
        ticket: DispatchTicket,
        cancel: CancellationToken,
    ) -> BoxPinFut<Result<ReconcileResult, BoxError>> {
        self.native.reconcile(ticket, cancel)
    }
}

async fn fixture() -> (
    Arc<crate::space::Space>,
    Arc<LearningRuntime>,
    Arc<crate::runtime::BusinessClock>,
    LearningConfig,
    PairedTrialPlan,
    String,
) {
    let (store, _, space, mut cfg, mut plan, basis) =
        setup_store_with_procedure(Arc::new(object_store::memory::InMemory::new()), WRONG).await;
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

fn freeze_cases(
    plan: &mut PairedTrialPlan,
    label: &str,
    requirements: &[Requirement],
) -> BTreeMap<String, Requirement> {
    assert_eq!(plan.pairs.len(), requirements.len());
    plan.pairs
        .iter_mut()
        .zip(requirements)
        .map(|((id, case), requirement)| {
            case.initial_state_digest = world::initial_state_digest(*requirement);
            case.task_digest =
                content_digest(&json!({"goal":"commit the work item", "id":id,"window":label}))
                    .unwrap();
            case.seed = format!("101:{label}:{id}");
            (id.clone(), *requirement)
        })
        .collect()
}

fn executor(
    space: &crate::space::Space,
    cfg: &LearningConfig,
    plan: &PairedTrialPlan,
    cases: BTreeMap<String, Requirement>,
) -> Arc<InstrumentedExecutor> {
    Arc::new(InstrumentedExecutor {
        native: Executor::new(cfg, plan, space.memory.nexus()),
        cases,
        receipts: Default::default(),
    })
}

async fn measured_cohort(
    rt: &Arc<LearningRuntime>,
    job: &str,
    executor: Arc<InstrumentedExecutor>,
) -> (JobReport, Vec<Json>) {
    let mut trace = vec![];
    loop {
        match rt.drive(job.into(), executor.clone()).await.unwrap() {
            DriveResult::Dispatched(ticket) => {
                let verified = executor.receipts.lock()[&ticket.dispatch_id].clone();
                let calls = verified["costs"]["tools"]["calls"].as_u64().unwrap();
                let mut submission = outcome(&ticket, false);
                submission.measurements = OutcomeMeasurements {
                    finished: true,
                    accounting_complete: true,
                    first_commit_success: verified["first_commit_success"].as_bool(),
                    final_committed: verified["final_committed"].as_bool(),
                    failed_commits: verified["failed_commits"].as_u64(),
                    unsafe_actions: verified["unsafe_actions"].as_u64(),
                    tool_calls: Some(calls),
                    elapsed_ms: Some(calls),
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    journal_digest: verified["journal_digest"].as_str().unwrap().into(),
                };
                let reported = rt.submit_outcome(auth(), submission).await.unwrap();
                trace.push(json!({"pair_id":ticket.pair_id,"arm":ticket.arm,"attempt_ref":ticket.attempt_ref,
                    "outcome_cursor":reported.outcome_cursor,"measurement":verified}));
            }
            DriveResult::Ready(report) => return (report, trace),
            other => panic!("unexpected instrumented state: {other:?}"),
        }
    }
}

async fn skill(space: &crate::space::Space) -> String {
    command(
        &space.memory.nexus(),
        r#"FIND(?s.id) WHERE {?s CONCEPT {type:"Skill"}} LIMIT 1"#,
    )
    .await[0]
        .as_str()
        .unwrap()
        .into()
}

fn context(executor: &InstrumentedExecutor, plan: &PairedTrialPlan) -> ApplicationContext {
    let now = anda_engine::unix_ms();
    ApplicationContext {
        executor: executor.identity(),
        revision_ref: plan.candidate_revision.clone(),
        preconditions_satisfied: true,
        evidence_digest: content_digest(&json!("public host context, not a hidden world label"))
            .unwrap(),
        checked_at_ms: now,
        expires_at_ms: now + 30_000,
    }
}

#[tokio::test]
async fn p6_early_success_cannot_filter_later_harm_out_of_a_frozen_cohort() {
    let (space, rt, clock, cfg, mut plan, basis) = fixture().await;
    let cases = freeze_cases(
        &mut plan,
        "mixed",
        &[
            Requirement::Required,
            Requirement::Required,
            Requirement::Required,
            Requirement::Required,
            Requirement::Forbidden,
            Requirement::Forbidden,
            Requirement::Forbidden,
            Requirement::Forbidden,
        ],
    );
    let exe = executor(&space, &cfg, &plan, cases);
    rt.enroll("mixed".into(), plan.clone(), basis)
        .await
        .unwrap();
    let (_, trace) = measured_cohort(&rt, "mixed", exe).await;
    assert!(
        rt.settle("mixed".into()).await.is_err(),
        "no early stopping/peeking"
    );
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let settled = rt.settle("mixed".into()).await.unwrap();
    let evaluation = rt
        .record(
            settled.evaluation_ref.as_deref().unwrap(),
            "EvaluationRecord",
        )
        .await
        .unwrap();
    assert_ne!(evaluation["comparison"]["status"], "improved");
    assert_ne!(evaluation["to_status"], "adopted");
    let treatment: Vec<_> = trace
        .iter()
        .filter(|r| r["arm"]["arm"] == "treatment")
        .collect();
    assert_eq!(treatment.len(), 8);
    assert!(
        treatment[..4]
            .iter()
            .all(|r| r["measurement"]["outcome_status"] == "success")
    );
    assert!(
        treatment[4..]
            .iter()
            .all(|r| r["measurement"]["unsafe_actions"] == 1)
    );
    assert!(
        !rt.procedure_status(&skill(&space).await)
            .await
            .unwrap()
            .recommendation_allowed
    );
    println!(
        "P6_NATIVE_TRACE {}",
        json!({"format":"p6-native-mechanism/0.1","scenario":"wrong-generalization", "fixture":true,
        "learning_effectiveness":"not_evaluated","plan":plan,"trace":trace,"evaluation":evaluation})
    );
    rt.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn p6_unannounced_drift_revokes_and_requires_a_fresh_trial_for_reentry() {
    let (space, rt, clock, cfg, mut plan, basis) = fixture().await;
    let skill_ref = skill(&space).await;
    let cases = freeze_cases(&mut plan, "acquisition", &[Requirement::Required; 8]);
    let exe = executor(&space, &cfg, &plan, cases);
    rt.enroll("acquire".into(), plan.clone(), basis.clone())
        .await
        .unwrap();
    let (_, first_trace) = measured_cohort(&rt, "acquire", exe.clone()).await;
    clock
        .advance_to(time_ms(&plan.execution.cutoff).unwrap())
        .unwrap();
    let adopted = rt.settle("acquire".into()).await.unwrap();
    let original = adopted.evaluation_ref.clone().unwrap();
    assert_eq!(
        rt.record(&original, "EvaluationRecord").await.unwrap()["to_status"],
        "adopted"
    );
    rt.bind_application_context(&AuthContext::system(), Some(context(&exe, &plan)))
        .unwrap();
    assert!(
        rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );

    // The drift does not change the model-visible task, public environment pin,
    // host context or configuration. Only actual tool execution reveals it.
    let mut review = plan.clone();
    let cases = freeze_cases(&mut review, "monitoring", &[Requirement::Forbidden; 8]);
    assert_eq!(review.environment_digest, plan.environment_digest);
    assert_eq!(review.model_digest, plan.model_digest);
    assert_eq!(review.budget_digest, plan.budget_digest);
    review.execution.review_of = Some(adopted.review.as_ref().unwrap().acquisition.clone());
    clock
        .advance_to(time_ms(&plan.execution.review_due_at).unwrap())
        .unwrap();
    assert!(
        !rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );
    review.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
    review.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
    let monitoring = executor(&space, &cfg, &review, cases);
    rt.enroll_review(
        "acquire".into(),
        "monitor".into(),
        review.clone(),
        basis.clone(),
    )
    .await
    .unwrap();
    let (_, drift_trace) = measured_cohort(&rt, "monitor", monitoring.clone()).await;
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
    assert_eq!(verdict["to_status"], "revoked");
    assert!(
        !rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );
    let sent = monitoring.native.seen.lock().len();
    let _ = rt.drive("monitor".into(), monitoring.clone()).await;
    assert_eq!(
        sent,
        monitoring.native.seen.lock().len(),
        "closed trial cannot dispatch again after revocation"
    );
    assert_eq!(
        rt.record(&original, "EvaluationRecord").await.unwrap()["to_status"],
        "adopted",
        "immutable historical grade"
    );

    let mut retry = plan.clone();
    let cases = freeze_cases(&mut retry, "new-trial", &[Requirement::Required; 8]);
    retry.execution.cutoff = crate::kip::timestamp(clock.now_ms() + 120_000);
    retry.execution.review_due_at = crate::kip::timestamp(clock.now_ms() + 240_000);
    let new_executor = executor(&space, &cfg, &retry, cases);
    let newly_enrolled = rt
        .enroll("retry".into(), retry.clone(), basis)
        .await
        .unwrap();
    assert!(newly_enrolled.evaluation_ref.is_none());
    assert!(
        !rt.procedure_status(&skill_ref)
            .await
            .unwrap()
            .recommendation_allowed
    );
    let (_, retry_trace) = measured_cohort(&rt, "retry", new_executor).await;
    clock
        .advance_to(time_ms(&retry.execution.cutoff).unwrap())
        .unwrap();
    let readopted = rt.settle("retry".into()).await.unwrap();
    assert_ne!(readopted.trial_ref, adopted.trial_ref);
    assert_ne!(readopted.evaluation_ref.as_ref(), Some(&original));
    assert_eq!(
        rt.record(
            readopted.evaluation_ref.as_deref().unwrap(),
            "EvaluationRecord"
        )
        .await
        .unwrap()["to_status"],
        "adopted"
    );
    println!(
        "P6_NATIVE_TRACE {}",
        json!({"format":"p6-native-mechanism/0.1","scenario":"unannounced-drift", "fixture":true,
        "learning_effectiveness":"not_evaluated","plans":[plan,review,retry],"traces":[first_trace,drift_trace,retry_trace],
        "acquisition":adopted,"revocation":revoked,"reentry":readopted,"post_revocation_dispatches":monitoring.native.seen.lock().len()-sent})
    );
    rt.shutdown().await;
    space.close().await.unwrap();
}
