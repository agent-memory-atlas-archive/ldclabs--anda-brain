//! An actual loopback business server with disk-backed world/journal state.
//! Only its model endpoint is deterministic; production transport, admission,
//! reset/tools, independent authenticated reads and native records are real.
use super::*;
use crate::learning::{workflow_http::*, *};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::any,
};
use std::io::Write;

struct Host {
    config: LearningConfig,
    identity: ExecutorIdentity,
    enrollment: LearningEnrollment,
    root: std::path::PathBuf,
    states: BTreeMap<String, (Json, WorkflowState)>,
    lock: Mutex<()>,
    resets: std::sync::atomic::AtomicU64,
    model_inputs: parking_lot::Mutex<Vec<Json>>,
    valid_capabilities: AtomicBool,
    hold_decide: AtomicBool,
    decide_entered: tokio::sync::Notify,
    release_decide: tokio::sync::Notify,
}
impl Host {
    fn path(&self, dispatch: &str) -> std::path::PathBuf {
        self.root.join(format!(
            "{}.json",
            &content_digest(&json!(dispatch)).unwrap()[7..]
        ))
    }
    fn read(&self, dispatch: &str) -> Result<Option<Json>, BoxError> {
        match std::fs::read(self.path(dispatch)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn save(&self, dispatch: &str, value: &Json) -> Result<(), BoxError> {
        let path = self.path(dispatch);
        let tmp = path.with_extension("pending");
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&serde_json::to_vec(value)?)?;
            // Windows FlushFileBuffers requires a writable handle. Sync the
            // writer itself, then close it before replacing the prior snapshot.
            file.sync_all()?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}
async fn endpoint(
    State(host): State<Arc<Host>>,
    Path((role, op)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::Json<Json>, (StatusCode, String)> {
    let expected = match role.as_str() {
        "executor" => "Bearer executor-secret",
        "observer" => "Bearer observer-secret",
        "source" => "Bearer source-secret",
        _ => return Err((StatusCode::NOT_FOUND, "role".into())),
    };
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(expected) {
        return Err((StatusCode::UNAUTHORIZED, "identity".into()));
    }
    let input = if body.is_empty() {
        Json::Null
    } else {
        serde_json::from_slice(&body).map_err(|_| (StatusCode::BAD_REQUEST, "json".into()))?
    };
    if role == "executor" && op == "decide" && host.hold_decide.swap(false, Ordering::SeqCst) {
        host.decide_entered.notify_one();
        host.release_decide.notified().await;
    }
    let _g = host.lock.lock().await;
    let result: Result<Json, BoxError> = (|| {
        match (role.as_str(), op.as_str()) {
            ("executor", "capabilities") => {
                return Ok(json!(WorkflowCapabilities {
                    format: "anda-brain:workflow-http-v1".into(),
                    identity: host.identity.clone(),
                    reset_isolated: true,
                    request_idempotency: true,
                    authoritative_status: true,
                    fence_and_deadline_enforced: true,
                    cancellation: host.valid_capabilities.load(Ordering::SeqCst),
                    complete_instrumented_journal: true,
                    calibration_digest: host.config.calibration_digest.clone()
                }));
            }
            ("observer", "identity") => return Ok(json!(host.config.observer)),
            ("source", "enrollment") => {
                return Ok(
                    if input["review"].is_null()
                        && input["after"] != host.enrollment.origin.event_key
                    {
                        json!(host.enrollment)
                    } else {
                        Json::Null
                    },
                );
            }
            _ => {}
        }
        let request = input.get("request").unwrap_or(&input);
        let dispatch = request["dispatch_id"]
            .as_str()
            .ok_or("dispatch id absent")?;
        let old = host.read(dispatch)?;
        if role == "observer" && op == "journal" {
            return Ok(old.map(|v| v["journal"].clone()).unwrap_or(Json::Null));
        }
        if role != "executor" {
            return Err("role cannot mutate this host".into());
        }
        if op == "status" {
            return Ok(json!(WorkflowStatus {
                request_digest: content_digest(request)?,
                state: if old
                    .as_ref()
                    .is_some_and(|v| v["journal"]["finished"] == true)
                {
                    ReconcileResult::Finished
                } else if old.is_some() {
                    ReconcileResult::Running
                } else {
                    ReconcileResult::NotStarted
                }
            }));
        }
        if op == "reset" {
            if old.is_some() {
                return Err("test catches any repeated reset".into());
            }
            let case: PairCase = serde_json::from_value(request["case"].clone())?;
            let (task, state) = host
                .states
                .get(&case.task_digest)
                .ok_or("unknown actual task")?;
            if request["fencing_token"].as_u64().unwrap_or(0) == 0
                || request["deadline_ms"].as_u64().unwrap_or(0) <= anda_engine::unix_ms()
            {
                return Err("expired host fence".into());
            }
            let journal = WorkflowJournal {
                format: "anda-brain:workflow-http-v1".into(),
                dispatch_id: dispatch.into(),
                attempt_ref: request["attempt_ref"].as_str().unwrap().into(),
                fencing_token: request["fencing_token"].as_u64().unwrap(),
                identity: host.identity.clone(),
                case,
                initial_state: state.clone(),
                events: vec![],
                started_at_ms: anda_engine::unix_ms(),
                finished_at_ms: None,
                finished: false,
                accounting_complete: true,
                input_tokens: Some(0),
                output_tokens: Some(0),
            };
            host.save(
                dispatch,
                &json!({"request":request,"state":state,"journal":journal,"replies":{}}),
            )?;
            host.resets.fetch_add(1, Ordering::SeqCst);
            return Ok(json!(WorkflowReset {
                request_digest: content_digest(request)?,
                identity: host.identity.clone(),
                initial_state_digest: content_digest(&json!(state))?,
                task: task.clone()
            }));
        }
        let mut row = old.ok_or("dispatch was not reset")?;
        if op == "cancel" {
            row["journal"]["finished"] = true.into();
            row["journal"]["finished_at_ms"] = anda_engine::unix_ms().into();
            host.save(dispatch, &row)?;
            return Ok(json!({"cancelled":true}));
        }
        if row["request"]["deadline_ms"].as_u64().unwrap() <= anda_engine::unix_ms() {
            return Err("host refuses expired work".into());
        }
        if op == "decide" {
            host.model_inputs.lock().push(input.clone());
            assert!(
                input.get("case").is_none()
                    && input.get("seed").is_none()
                    && input.get("arm").is_none()
                    && input.get("review").is_none()
            );
            assert_eq!(
                input["base_memory_digest"],
                host.identity.base_memory_digest
            );
            let feedback = input["feedback"].as_array().unwrap();
            let action = if input["procedure"].is_null() {
                WorkflowAction::Commit
            } else if feedback.is_empty() {
                WorkflowAction::Inspect
            } else if feedback.len() == 1 && feedback[0]["reply"]["requirement"] == "required" {
                WorkflowAction::Prepare
            } else {
                WorkflowAction::Commit
            };
            row["journal"]["input_tokens"] =
                (row["journal"]["input_tokens"].as_u64().unwrap() + 7).into();
            row["journal"]["output_tokens"] =
                (row["journal"]["output_tokens"].as_u64().unwrap() + 3).into();
            host.save(dispatch, &row)?;
            return Ok(json!(WorkflowChoice {
                action: Some(action),
                input_tokens: 7,
                output_tokens: 3
            }));
        }
        if content_digest(&row["request"])? != content_digest(request)? {
            return Err("business host request pin mismatch".into());
        }
        if op == "finish" {
            row["journal"]["finished"] = true.into();
            row["journal"]["finished_at_ms"] = anda_engine::unix_ms().into();
            host.save(dispatch, &row)?;
            return Ok(json!({"sealed":true}));
        }
        let action: WorkflowAction = serde_json::from_value(json!(op))?;
        let sequence = input["sequence"].as_u64().ok_or("missing sequence")?;
        let key = sequence.to_string();
        if let Some(reply) = row["replies"].get(&key) {
            return Ok(reply.clone());
        }
        let before: WorkflowState = serde_json::from_value(row["state"].clone())?;
        let mut after = before.clone();
        let mut succeeded = true;
        let public = match action {
            WorkflowAction::Inspect => json!({"requirement":before.preparation_requirement}),
            WorkflowAction::Prepare => {
                after.prepared = true;
                json!({"prepared":true})
            }
            WorkflowAction::Commit => {
                succeeded = before.preparation_requirement != WorkflowRequirement::Required
                    || before.prepared;
                after.committed = succeeded;
                json!({"committed":succeeded})
            }
        };
        let reply = json!(WorkflowReply {
            public,
            terminal: action == WorkflowAction::Commit
        });
        row["state"] = json!(after);
        row["journal"]["events"]
            .as_array_mut()
            .unwrap()
            .push(json!(WorkflowEvent {
                sequence,
                action,
                before,
                after,
                succeeded
            }));
        row["replies"][key] = reply.clone();
        host.save(dispatch, &row)?;
        Ok(reply)
    })();
    result
        .map(axum::Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

#[tokio::test]
async fn r5_real_startup_http_adapter_resets_disk_state_observes_independently_and_archives() {
    let (store, _, seeded, mut cfg, mut plan, basis) = setup().await;
    cfg.reconcile_timeout_ms = 1000;
    cfg.maximum_jobs = 1;
    let calibration = json!(LearningCalibration {
        format: "anda-brain:learning-calibration-v1".into(),
        reviewed_by: "kip:principal:test-operator".into(),
        approved_for_automatic_trials: true,
        contract_digest: calibration_contract(&cfg).unwrap(),
        environment_digest: plan.environment_digest.clone(),
        training_manifest: json!({"fixture":"train"}),
        validation_manifest: json!({"fixture":"validation"}),
        report: json!({"kind":"mechanism-test-only","empirical_improvement":false})
    });
    cfg.calibration_digest = content_digest(&calibration).unwrap();
    plan.execution.cutoff = crate::kip::timestamp(anda_engine::unix_ms() + 120_000);
    plan.execution.review_due_at = crate::kip::timestamp(anda_engine::unix_ms() + 240_000);
    let mut states = BTreeMap::new();
    for (id, case) in &mut plan.pairs {
        let task = json!({"goal":"commit this registered work item","opaque_id":id});
        let state = WorkflowState {
            preparation_requirement: WorkflowRequirement::Required,
            prepared: false,
            committed: false,
        };
        case.task_digest = content_digest(&task).unwrap();
        case.initial_state_digest = content_digest(&json!(state)).unwrap();
        states.insert(case.task_digest.clone(), (task, state));
    }
    let identity = Executor::new(&cfg, &plan, seeded.memory.nexus()).identity();
    let origin = EnrollmentOrigin {
        source_id: "registered-business-source".into(),
        source_digest: content_digest(&json!("pinned-source-contract")).unwrap(),
        event_key: "frozen-run-1".into(),
        trigger_ref: None,
        gate_wake_ref: None,
    };
    let root = std::env::temp_dir().join(format!("brain-r5-business-{}", rand::random::<u64>()));
    std::fs::create_dir(&root).unwrap();
    let host = Arc::new(Host {
        config: cfg.clone(),
        identity: identity.clone(),
        enrollment: LearningEnrollment {
            job_id: "http-workflow".into(),
            plan: plan.clone(),
            basis_proposition: basis,
            origin: origin.clone(),
        },
        root: root.clone(),
        states,
        lock: Mutex::new(()),
        resets: 0.into(),
        model_inputs: Default::default(),
        valid_capabilities: AtomicBool::new(true),
        hold_decide: AtomicBool::new(false),
        decide_entered: Default::default(),
        release_decide: Default::default(),
    });
    // Surface disk errors directly before exercising background HTTP dispatch;
    // otherwise a failed reset only appears as a timeout waiting for `decide`.
    for value in [
        json!({"state": "initial snapshot"}),
        json!({"state": "new"}),
    ] {
        host.save("storage-preflight", &value)
            .expect("business host must persist and replace its disk snapshot");
        assert_eq!(host.read("storage-preflight").unwrap(), Some(value));
        assert!(
            !host
                .path("storage-preflight")
                .with_extension("pending")
                .exists()
        );
    }
    std::fs::remove_file(host.path("storage-preflight")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = tokio::spawn(
        axum::serve(
            listener,
            axum::Router::new()
                .route("/{role}/{op}", any(endpoint))
                .with_state(host.clone()),
        )
        .into_future(),
    );
    let http = WorkflowHttpConfig {
        id: "workflow_http_v1".into(),
        registration: cfg.clone(),
        identity,
        automation: LearningAutomation {
            trials: true,
            reviews: true,
            archive: true,
            safety: true,
        },
        storage: LearningStoragePolicy::default(),
        executor_endpoint: format!("http://{addr}/executor/"),
        observer_endpoint: format!("http://{addr}/observer/"),
        source_endpoint: format!("http://{addr}/source/"),
        executor_token_env: "R5_EXECUTOR".into(),
        observer_token_env: "R5_OBSERVER".into(),
        source_token_env: "R5_SOURCE".into(),
        source_id: origin.source_id,
        source_digest: origin.source_digest,
        calibration,
        callback_timeout_ms: 10_000,
    };
    let secrets = |name: &str| match name {
        "R5_EXECUTOR" => Some("executor-secret".into()),
        "R5_OBSERVER" => Some("observer-secret".into()),
        "R5_SOURCE" => Some("source-secret".into()),
        _ => None,
    };
    let runtime_config: crate::runtime_api::config::RuntimeConfig = serde_json::from_value(json!({"format":crate::runtime_api::FORMAT,
        "spaces":{"p3_space":{"bootstrap":true,"subjects":[],"audience":[],"observers":[],"adapter":null,"learning":http}}})).unwrap();
    seeded.close().await.unwrap();
    let app = app(store.clone())
        .with_runtime_config(runtime_config, secrets)
        .unwrap();
    let space = app.load_space_with("p3_space", false, false).await.unwrap();
    let rt = space.learning();
    let ingress = space.memory_runtime().unwrap().consequences();
    assert_eq!(
        host.resets.load(Ordering::SeqCst),
        0,
        "startup probes must not execute tasks"
    );
    let status = rt.runtime_status(true, true).await.unwrap();
    assert!(status.registered && status.bindings_ready && status.automatic_allowed);
    rt.kick(Some(ingress.clone()), false);
    assert!(rt.jobs().await.unwrap().is_empty());
    app.attention_tick().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !rt.scheduler_running.load(Ordering::SeqCst)
                && rt.jobs().await.is_ok_and(|jobs| !jobs.is_empty())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rt.runtime_status(true, true)
            .await
            .unwrap()
            .last_pass
            .unwrap()
            .enrolled,
        1
    );
    host.hold_decide.store(true, Ordering::SeqCst);
    space.attention().register_work().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), app.attention_tick())
        .await
        .unwrap()
        .unwrap();
    let entered = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        host.decide_entered.notified(),
    )
    .await;
    assert!(
        entered.is_ok(),
        "executor did not reach decide: resets={}, runtime={:?}",
        host.resets.load(Ordering::SeqCst),
        rt.runtime_status(true, true).await
    );
    assert!(
        rt.scheduler_running.load(Ordering::SeqCst),
        "attention tick returned while business I/O is still running"
    );
    host.release_decide.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while rt.scheduler_running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..5 {
        let pass = rt.automatic_pass(Some(ingress.clone())).await.unwrap();
        assert!(pass.blocked.is_empty(), "{pass:?}");
    }
    let ready = rt.report("http-workflow").await.unwrap();
    assert_eq!(ready.stage, JobStage::ReadyForEvaluation);
    assert_eq!(ready.outcome_cursor, 4);
    assert_eq!(host.resets.load(Ordering::SeqCst), 4);
    for a in &ready.attempts {
        let journal = rt
            .observation_replay("http-workflow", &a.dispatch_id)
            .await
            .unwrap()
            .unwrap();
        let typed: WorkflowJournal = serde_json::from_value(journal.clone()).unwrap();
        assert!(!typed.initial_state.prepared);
        assert!(!typed.initial_state.committed);
        assert!(typed.accounting_complete);
        let reg = rt.registration(false).await.unwrap();
        let frozen = rt.load_job(&reg, "http-workflow").await.unwrap().value;
        let attempt = frozen
            .attempts
            .values()
            .find(|v| v.ticket.dispatch_id == a.dispatch_id)
            .unwrap();
        let ticket = rt.hydrate_ticket(&frozen, attempt).await.unwrap();
        let mut missing_usage = typed.clone();
        missing_usage.input_tokens = None;
        assert_eq!(
            verify_journal(&ticket, &http.identity, &missing_usage)
                .unwrap()
                .classify(&plan)
                .unwrap(),
            NativeOutcomeStatus::Unknown
        );
        let mut invented_state = typed.clone();
        invented_state.events[0].after.committed = !invented_state.events[0].after.committed;
        assert!(verify_journal(&ticket, &http.identity, &invented_state).is_err());
        assert_eq!(
            typed.events.len(),
            if matches!(a.arm, NativeArm::Baseline) {
                1
            } else {
                3
            }
        );
        let outcome = rt
            .record(a.outcome_ref.as_deref().unwrap(), "OutcomeRecord")
            .await
            .unwrap();
        assert_eq!(
            outcome["outcome_status"],
            if matches!(a.arm, NativeArm::Baseline) {
                "failure"
            } else {
                "success"
            }
        );
        let decision = rt
            .record(
                &rt.load_job(&rt.registration(false).await.unwrap(), "http-workflow")
                    .await
                    .unwrap()
                    .value
                    .attempts
                    .values()
                    .find(|v| v.ticket.dispatch_id == a.dispatch_id)
                    .unwrap()
                    .ticket
                    .decision_ref,
                "DecisionRecord",
            )
            .await
            .unwrap();
        assert!(
            decision["rationale"]
                .as_str()
                .unwrap()
                .contains("frozen-run-1")
        );
        assert!(std::fs::metadata(host.path(&a.dispatch_id)).unwrap().len() > 0);
    }
    host.valid_capabilities.store(false, Ordering::SeqCst);
    assert!(
        http.resolve(secrets)
            .unwrap()
            .executor
            .preflight(CancellationToken::new())
            .await
            .is_err()
    );
    host.valid_capabilities.store(true, Ordering::SeqCst);
    space.close().await.unwrap();
    let db = Arc::new(
        anda_db::database::AndaDB::open(store.clone(), crate::testkit::db_config("p3_space"))
            .await
            .unwrap(),
    );
    let nexus = Arc::new(CognitiveNexus::connect(db.clone()).await.unwrap());
    let clock =
        crate::runtime::BusinessClock::manual(time_ms(&plan.execution.cutoff).unwrap()).unwrap();
    let recovered = LearningRuntime::connect(
        store.clone(),
        "p3_space/learning".into(),
        nexus.clone(),
        clock,
    )
    .await
    .unwrap();
    recovered
        .install_bindings(AuthContext::system(), http.resolve(secrets).unwrap())
        .await
        .unwrap();
    let native = nexus
        .system_session()
        .read_control(DEFAULT_SPACE, "attention/config", None)
        .await
        .unwrap()
        .unwrap();
    let scope =
        serde_json::from_value::<anda_cognitive_nexus::attention::AttentionConfig>(native.value)
            .unwrap()
            .scope;
    let recovered_ingress = crate::consequence::ConsequenceRuntime::new(
        nexus,
        crate::attention::Directory::new(store, 0),
        scope,
        vec![],
        false,
        Some(Arc::downgrade(&recovered)),
    );
    let last = recovered
        .automatic_pass(Some(recovered_ingress.clone()))
        .await
        .unwrap();
    assert_eq!(last.settled, 1, "{last:?}");
    assert_eq!(last.archived, 1, "{last:?}");
    assert_eq!(
        host.resets.load(Ordering::SeqCst),
        4,
        "restart and cutoff must not reset/resend a business task"
    );
    assert!(
        recovered
            .observation_replay("http-workflow", &ready.attempts[0].dispatch_id)
            .await
            .unwrap()
            .is_some()
    );
    recovered_ingress.shutdown().await;
    recovered.shutdown().await;
    db.close().await.unwrap();
    service.abort();
    let _ = service.await;
    std::fs::remove_dir_all(root).unwrap();
}
