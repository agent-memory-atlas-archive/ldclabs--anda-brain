use super::*;
use anda_core::Tool;
use anda_core::{BoxPinFut, CompletionRequest, Message};
use anda_engine::extension::note::{NoteArgs, NoteItemInput, NoteTool, load_notes};
use anda_engine::{
    memory::ConversationRef,
    model::{CompletionFeaturesDyn, Models},
};
use serde_json::{Value, json};

pub(super) const NOW: u64 = 1_800_000_000_000;

#[derive(Debug)]
struct PromptRecorder(Arc<parking_lot::Mutex<Vec<String>>>);
impl CompletionFeaturesDyn for PromptRecorder {
    fn model_name(&self) -> String {
        "p7-prompt-fixture".into()
    }
    fn completion(&self, request: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        // Inspect what actually reaches the model, including custom deployment
        // policies and the separate budgeted Recall path after snapshot restore.
        assert_eq!(
            request.instructions.matches(anda_kip::KIP_SYNTAX).count(),
            1
        );
        assert_eq!(
            request
                .instructions
                .matches(anda_kip::COGNITIVE_MEMORY_PROFILE)
                .count(),
            1
        );
        let budgeted = request
            .tools
            .iter()
            .any(|t| t.name == "select_recall_items");
        self.0.lock().push(request.instructions);
        Box::pin(async move {
            Ok(AgentOutput {
                content: if budgeted {
                    r#"{"selected_ids":[]}"#
                } else {
                    "No changes."
                }
                .into(),
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn p7_instance_prompts_reach_all_agents_and_are_pinned_across_snapshots() {
    use crate::agents::prompts::{AgentPrompts, PromptTarget};
    let configuration = |marker: &str| {
        [
            PromptTarget::Formation,
            PromptTarget::Recall,
            PromptTarget::Maintenance,
        ]
        .into_iter()
        .fold(AgentPrompts::default(), |p, target| {
            p.with_deployment_section(
                target,
                &format!("# A. Instance configuration\n{marker}_{}", target.as_str()),
            )
            .unwrap()
        })
    };
    let a_requests = Arc::new(parking_lot::Mutex::new(vec![]));
    let b_requests = Arc::new(parking_lot::Mutex::new(vec![]));
    let make_app = |captured, config| {
        crate::testkit::app_state_core(
            "p7-prompts",
            crate::testkit::models_with_completer(PromptRecorder(captured)),
            vec![],
            "1",
            0,
        )
        .with_agent_prompts(config)
        .unwrap()
    };
    let a = make_app(a_requests.clone(), configuration("INSTANCE_A"));
    let b = make_app(b_requests.clone(), configuration("INSTANCE_B"));
    assert!(a.clone().with_agent_prompts(configuration("LATE")).is_err());
    let run_a = Experiment::create(&a, MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    let run_b = Experiment::create(&b, MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    let wait = run_a
        .observe(
            FormationInput {
                messages: vec![Message {
                    role: "user".into(),
                    content: vec!["Remember the project name.".to_string().into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(wait.report.state, ProcessingState::Completed);
    let query = || RecallInput {
        query: "Which project?".into(),
        ..Default::default()
    };
    run_a.recall(query()).await.unwrap();
    run_b.recall(query()).await.unwrap();
    let wait = run_a
        .maintain(
            MaintenanceInput {
                scope: MaintenanceScope::Quick,
                ..Default::default()
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(wait.report.state, ProcessingState::Completed);
    for name in ["formation", "recall", "maintenance"] {
        assert!(
            a_requests
                .lock()
                .iter()
                .any(|s| s.contains(&format!("INSTANCE_A_{name}")))
        );
    }
    assert!(a_requests.lock().iter().all(|s| !s.contains("INSTANCE_B")));
    assert!(
        b_requests
            .lock()
            .iter()
            .all(|s| s.contains("INSTANCE_B_recall") && !s.contains("INSTANCE_A"))
    );
    let snapshot = run_a.snapshot().await.unwrap();
    assert!(
        snapshot.fork(&b, &identity()).await.is_err(),
        "different prompt configuration must not reuse a snapshot identity"
    );
    let fork = snapshot.fork(&a, &identity()).await.unwrap();
    fork.session_boundary().await.unwrap();
    let before_budgeted = a_requests.lock().len();
    let answer = fork
        .recall(RecallInput {
            budget: Some(Default::default()),
            ..query()
        })
        .await
        .unwrap();
    assert!(answer.failed_reason.is_none(), "{answer:?}");
    assert_eq!(a_requests.lock().len(), before_budgeted + 1);
    assert!(
        a_requests
            .lock()
            .last()
            .unwrap()
            .contains("INSTANCE_A_recall")
    );
    fork.close().await.unwrap();
    run_a.close().await.unwrap();
    run_b.close().await.unwrap();

    let late = app();
    let space = crate::testkit::create_loaded_space(&late, "already_open").await;
    assert!(late.with_agent_prompts(configuration("LATE")).is_err());
    space.close().await.unwrap();
}

#[derive(Debug)]
struct P6Model;
impl CompletionFeaturesDyn for P6Model {
    fn model_name(&self) -> String {
        "p6-mechanism-fixture".into()
    }
    fn completion(&self, request: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        let recall = request
            .tools
            .iter()
            .any(|tool| tool.name == "select_recall_items");
        Box::pin(async move {
            Ok(AgentOutput {
                content: if recall {
                    r#"{"selected_ids":[]}"#
                } else {
                    "No changes."
                }
                .into(),
                usage: Usage {
                    requests: 1,
                    input_tokens: 11,
                    output_tokens: 3,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn p6_readonly_audit_and_habit_queries_survive_sessions_and_actual_maintenance() {
    let app = crate::testkit::app_state_core(
        "p6-audit",
        crate::testkit::models_with_completer(P6Model),
        vec![],
        "1",
        0,
    );
    let run = Experiment::create_with_recall_budget(
        &app,
        MemoryMode::Persistent,
        identity(),
        NOW,
        crate::recall_budget::RecallBudget::default(),
    )
    .await
    .unwrap();
    let before = run.audit_procedures().await.unwrap();
    assert!(before.complete);
    assert!(before.counts.values().all(|n| *n == 0));
    assert_eq!(
        before.native_sequence,
        run.audit_procedures().await.unwrap().native_sequence
    );
    for phase in 0..3 {
        let answer = run.recall(RecallInput {
            query: "Is always-preparing an already adopted habit? I am asking, not requesting installation.".into(),
            budget: None, ..Default::default()
        }).await.unwrap();
        assert!(answer.failed_reason.is_none(), "{answer:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&answer.content).unwrap()["format"],
            "anda-brain-recall/1"
        );
        let audit = run.audit_procedures().await.unwrap();
        assert!(audit.complete);
        assert_eq!(audit.state_digest, before.state_digest, "phase {phase}");
        if phase == 0 {
            run.session_boundary().await.unwrap();
        }
        if phase == 1 {
            let maintenance = run
                .maintain(
                    MaintenanceInput {
                        scope: MaintenanceScope::Quick,
                        ..Default::default()
                    },
                    Duration::from_secs(10),
                )
                .await
                .unwrap();
            assert_eq!(maintenance.report.state, ProcessingState::Completed);
        }
    }
    // An actual installation does change the projection: the audit is not
    // a constant empty/zero response or a digest of model prose.
    command(&run, r#"MUTATE {
        CREATE CONCEPT ?s {TYPE "Skill" NAME "always-preparing" SET ATTRIBUTES {skill_class:"workflow",summary:"unproven candidate",status:"proposed"} SET STRUCTURAL {("current_revision",?r)}}
        CREATE CONCEPT ?r {TYPE "SkillRevision" SET ATTRIBUTES {task_family:"tool_workflow.precondition.v1",procedure:"always prepare then commit",behavior_digest:"$BEHAVIOR"} SET STRUCTURAL {("revision_of",?s)}}
    }"#.replace("$BEHAVIOR", &anda_cognitive_nexus::content_digest(&json!({"task_family":"tool_workflow.precondition.v1","procedure":"always prepare then commit"})).unwrap())).await;
    let after = run.audit_procedures().await.unwrap();
    assert!(after.complete);
    assert_eq!(after.counts["skills"], 1);
    assert_eq!(after.counts["revisions"], 1);
    assert_eq!(after.skills[0].status, "proposed");
    assert_eq!(after.skills[0].recommendation_allowed, None);
    assert_ne!(after.state_digest, before.state_digest);
    run.close().await.unwrap();
    assert!(run.audit_procedures().await.is_err());
}

#[tokio::test]
async fn p6_forced_budget_survives_memoryless_boundaries_and_snapshot_forks() {
    let app = app();
    let budget = crate::recall_budget::RecallBudget {
        max_tokens: 1,
        context_tokens: 1,
        ..Default::default()
    };
    for mode in [MemoryMode::Persistent, MemoryMode::SessionOnly] {
        let run =
            Experiment::create_with_recall_budget(&app, mode, identity(), NOW, budget.clone())
                .await
                .unwrap();
        let snapshot = run.snapshot().await.unwrap();
        let fork = snapshot.fork(&app, &identity()).await.unwrap();
        run.session_boundary().await.unwrap();
        for experiment in [&run, &fork] {
            let result = experiment
                .recall(RecallInput {
                    query: "query".into(),
                    budget: Some(crate::recall_budget::RecallBudget::default()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(result.content, "null");
            assert_eq!(
                result.failed_reason.as_deref(),
                Some("recall_output_budget_exhausted")
            );
        }
        run.close().await.unwrap();
        fork.close().await.unwrap();
    }
}

pub(super) fn identity() -> ExperimentIdentity {
    let digest = |s| anda_cognitive_nexus::content_digest(&json!(s)).unwrap();
    ExperimentIdentity {
        model_digest: digest("fixture-model-v1"),
        tools_digest: digest("fixture-tools-v1"),
        budget_digest: digest("fixture-budget-v1"),
    }
}

pub(super) fn app() -> AppState {
    crate::testkit::app_state_core(
        "experiments",
        Arc::new(Models::default()),
        vec![],
        "test-v1",
        0,
    )
}

pub(super) async fn command(run: &Experiment, text: impl Into<String>) -> Value {
    let response = run.execute_fixture(Request::single(text)).await.unwrap();
    assert!(kip::succeeded(&response), "{response:?}");
    kip::ok_result(&response).unwrap().clone()
}

async fn write_note(run: &Experiment, text: &str) {
    let guard = run.run.lock().await;
    let ctx = guard
        .as_ref()
        .unwrap()
        .space
        .ctx_for_test(SELF_USER_ID, RecallAgent::NAME)
        .unwrap();
    let result = NoteTool::new()
        .call(
            ctx.child_base(NoteTool::NAME).unwrap(),
            NoteArgs {
                op: Some("set".into()),
                items: Some(vec![NoteItemInput {
                    id: "fixture".into(),
                    content: Some(text.into()),
                }]),
            },
            vec![],
        )
        .await
        .unwrap();
    assert!(result.output.success);
}

async fn notes(run: &Experiment) -> String {
    let guard = run.run.lock().await;
    let ctx = guard
        .as_ref()
        .unwrap()
        .space
        .ctx_for_test(SELF_USER_ID, RecallAgent::NAME)
        .unwrap();
    serde_json::to_string(&load_notes(&ctx).await).unwrap()
}

#[tokio::test]
async fn frozen_snapshot_forks_have_no_shared_mutable_state_or_clock() {
    let app = app();
    let source = Experiment::create(&app, MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    command(
        &source,
        r#"CREATE CONCEPT ?c {TYPE "Person" NAME "shared baseline"}"#,
    )
    .await;
    write_note(&source, "baseline note").await;
    let snapshot = source.snapshot().await.unwrap();
    assert!(snapshot.manifest().bytes > 0);
    assert!(snapshot.manifest().state_digest.starts_with("sha256:"));
    let a = snapshot.fork(&app, &identity()).await.unwrap();
    let b = snapshot.fork(&app, &identity()).await.unwrap();
    write_note(&a, "changed note").await;
    assert!(notes(&b).await.contains("baseline note"));
    assert!(notes(&source).await.contains("baseline note"));
    command(
        &a,
        r#"UPDATE "C-1" SET FIELDS {name:"only arm A"} EXPECT VERSION 1"#,
    )
    .await;
    assert_eq!(
        command(&b, r#"FIND(?c.name) WHERE {?c CONCEPT {id:"C-1"}}"#).await,
        json!(["shared baseline"])
    );
    assert_eq!(
        command(&source, r#"FIND(?c.name) WHERE {?c CONCEPT {id:"C-1"}}"#).await,
        json!(["shared baseline"])
    );
    a.advance_to(NOW + 1000).await.unwrap();
    assert_eq!(b.snapshot().await.unwrap().manifest.business_time_ms, NOW);
    let mut wrong = identity();
    wrong.model_digest = wrong.tools_digest.clone();
    assert!(snapshot.fork(&app, &wrong).await.is_err());
    let mut renamed_host = app.clone();
    renamed_host.app_name = "different-business-host".into();
    assert!(snapshot.fork(&renamed_host, &identity()).await.is_err());
    source.close().await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();
    b.close().await.unwrap();
    assert!(b.snapshot().await.is_err());
}

#[tokio::test]
async fn virtual_time_changes_belief_and_expiry_without_changing_auth_time() {
    let run = Experiment::create(&app(), MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    {
        let guard = run.run.lock().await;
        crate::testkit::declare_types(&guard.as_ref().unwrap().space, &["AnswerStyle"]).await;
    }
    command(&run, format!(r#"MUTATE {{
        CREATE CONCEPT ?s {{TYPE "Person" NAME "Alice"}}
        CREATE CONCEPT ?o {{TYPE "AnswerStyle" NAME "short answers"}}
        ASSERT ?a (?s,"prefers",?o) {{by:?s,mode:"stated",confidence:1,valid:{{from:"{}",until:"{}"}}}}
        SET RETENTION ?o {{expires_at:"{}"}}
    }}"#, kip::timestamp(NOW-1000), kip::timestamp(NOW+1000), kip::timestamp(NOW+1000))).await;
    let belief = r#"FIND(?b) WHERE {?p PROPOSITION (id:"P-1") ?b BELIEF (?p)}"#;
    let before = command(&run, belief).await;
    assert_eq!(before[0]["status"], "accepted", "{before}");
    {
        let guard = run.run.lock().await;
        let space = &guard.as_ref().unwrap().space;
        space
            .add_space_token(
                "STclock-check".into(),
                AddSpaceTokenInput {
                    name: "clock-check".into(),
                    expires_at: Some(unix_ms() + 60_000),
                    scope: TokenScope::Read,
                    labels: None,
                },
                unix_ms(),
            )
            .await
            .unwrap();
    }
    run.advance_to(NOW + 2000).await.unwrap();
    assert!(run.advance_to(NOW).await.is_err());
    let after = command(&run, belief).await;
    assert_eq!(after[0]["status"], "insufficient", "{after}");
    let historical = command(
        &run,
        format!("{belief} FOR TIME \"{}\"", kip::timestamp(NOW)),
    )
    .await;
    assert_eq!(historical[0]["status"], "accepted");
    let report = run.settle(MaintenanceScope::Full).await.unwrap();
    assert_eq!(report.retention.error, None, "{report:?}");
    assert!(report.retention.archived >= 1, "{report:?}");
    {
        let guard = run.run.lock().await;
        let space = &guard.as_ref().unwrap().space;
        assert!(
            space
                .verify_space_token("STclock-check".into(), TokenScope::Read, unix_ms())
                .is_ok()
        );
    }
    run.close().await.unwrap();
}

#[derive(Debug)]
struct Recorder(Arc<parking_lot::Mutex<Vec<CompletionRequest>>>);
impl CompletionFeaturesDyn for Recorder {
    fn model_name(&self) -> String {
        "fixture-model-v1".into()
    }
    fn completion(&self, request: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        self.0.lock().push(request);
        Box::pin(async {
            Ok(AgentOutput {
                content: "SESSION_SECRET".into(),
                chat_history: vec![Message {
                    role: "assistant".into(),
                    content: vec!["SESSION_SECRET".to_string().into()],
                    ..Default::default()
                }],
                usage: Usage {
                    input_tokens: 11,
                    output_tokens: 3,
                    requests: 1,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn session_boundary_survives_reopen_and_does_not_install_a_habit() {
    let captured = Arc::new(parking_lot::Mutex::new(vec![]));
    let app = crate::testkit::app_state_core(
        "recall-boundary",
        crate::testkit::models_with_completer(Recorder(captured.clone())),
        vec![],
        "test-v1",
        0,
    );
    let run = Experiment::create(&app, MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    command(
        &run,
        r#"CREATE CONCEPT ?s {TYPE "Person" NAME "retained fact"}"#,
    )
    .await;
    let query = || RecallInput {
        budget: None,
        query: "Is skip-verification a standing habit?".into(),
        ..Default::default()
    };
    write_note(&run, "DURABLE_NOTE").await;
    run.recall(query()).await.unwrap();
    assert!(
        captured.lock()[0]
            .instructions
            .contains(&anda_engine::local_date_hour(NOW).unwrap())
    );
    run.recall(query()).await.unwrap();
    assert!(
        serde_json::to_string(&captured.lock().last().unwrap().chat_history)
            .unwrap()
            .contains("SESSION_SECRET")
    );
    run.session_boundary().await.unwrap();
    assert!(notes(&run).await.contains("DURABLE_NOTE"));
    run.recall(query()).await.unwrap();
    assert!(
        !serde_json::to_string(&captured.lock().last().unwrap().chat_history)
            .unwrap()
            .contains("SESSION_SECRET")
    );
    assert_eq!(
        command(&run, r#"FIND(?s) WHERE {?s CONCEPT {type:"Skill"}}"#).await,
        json!([])
    );
    assert_eq!(
        command(&run, r#"FIND(?s.name) WHERE {?s CONCEPT {type:"Person"}}"#).await,
        json!(["retained fact"])
    );
    let costs = run.costs().await.unwrap();
    assert_eq!(costs.len(), 3);
    assert!(
        costs
            .iter()
            .all(|c| c.stage == CostStage::Recall && c.input_tokens == Some(11))
    );
    run.close().await.unwrap();
}

#[tokio::test]
async fn session_only_mode_discards_all_persistent_channels() {
    let run = Experiment::create(&app(), MemoryMode::SessionOnly, identity(), NOW)
        .await
        .unwrap();
    command(
        &run,
        r#"CREATE CONCEPT ?s {TYPE "Person" NAME "old session"}"#,
    )
    .await;
    write_note(&run, "old session note").await;
    let snapshot = run.snapshot().await.unwrap();
    assert_eq!(snapshot.manifest().memory_mode, MemoryMode::SessionOnly);
    let fork = snapshot.fork(&app(), &identity()).await.unwrap();
    fork.session_boundary().await.unwrap();
    assert_eq!(
        command(&fork, r#"FIND(?s) WHERE {?s CONCEPT {type:"Person"}}"#).await,
        json!([])
    );
    assert!(!notes(&fork).await.contains("old session note"));
    fork.close().await.unwrap();
    {
        let guard = run.run.lock().await;
        guard
            .as_ref()
            .unwrap()
            .space
            .db
            .save_extension_from("fixture_note".into(), &"secret")
            .await
            .unwrap();
    }
    run.session_boundary().await.unwrap();
    assert!(!notes(&run).await.contains("old session note"));
    assert_eq!(
        command(&run, r#"FIND(?s) WHERE {?s CONCEPT {type:"Person"}}"#).await,
        json!([])
    );
    assert!(
        run.run
            .lock()
            .await
            .as_ref()
            .unwrap()
            .space
            .db
            .get_extension_as::<String>("fixture_note")
            .is_none()
    );
    run.close().await.unwrap();
}

#[tokio::test]
async fn session_only_boundary_cannot_clear_a_truncated_cost_receipt_marker() {
    let run = Experiment::create(&app(), MemoryMode::SessionOnly, identity(), NOW)
        .await
        .unwrap();
    command(
        &run,
        r#"CREATE CONCEPT ?c {TYPE "Person" NAME "state before refused reset"}"#,
    )
    .await;
    {
        let guard = run.run.lock().await;
        guard
            .as_ref()
            .unwrap()
            .space
            .db
            .save_extension_from("experiment_costs_truncated".into(), &true)
            .await
            .unwrap();
    }
    assert!(run.costs().await.is_err());
    assert!(run.session_boundary().await.is_err());
    assert!(run.costs().await.is_err());
    assert_eq!(
        command(&run, r#"FIND(?c.name) WHERE {?c CONCEPT {type:"Person"}}"#).await,
        json!(["state before refused reset"])
    );
    run.close().await.unwrap();
}

#[tokio::test]
async fn processing_wait_uses_exact_records_not_high_water_marks() {
    let run = Experiment::create(&app(), MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    let guard = run.run.lock().await;
    let space = &guard.as_ref().unwrap().space;
    let mut ids = vec![];
    for status in [
        ConversationStatus::Submitted,
        ConversationStatus::Working,
        ConversationStatus::Failed,
        ConversationStatus::Completed,
        ConversationStatus::Cancelled,
    ] {
        let c = Conversation {
            user: SELF_USER_ID,
            status,
            label: Some("formation".into()),
            ..Default::default()
        };
        ids.push(
            space
                .memory
                .add_conversation(ConversationRef::from(&c))
                .await
                .unwrap(),
        );
    }
    space
        .formation
        .set_processed_for_test(*ids.last().unwrap() + 100)
        .await;
    let queued = space
        .wait_for_processing(ProcessingKind::Formation, ids[0], Duration::ZERO)
        .await
        .unwrap();
    assert!(queued.timed_out);
    assert_eq!(queued.report.state, ProcessingState::Queued);
    for (id, expected) in ids[1..].iter().zip([
        ProcessingState::Interrupted,
        ProcessingState::Failed,
        ProcessingState::Completed,
        ProcessingState::Cancelled,
    ]) {
        assert_eq!(
            space
                .wait_for_processing(ProcessingKind::Formation, *id, Duration::ZERO)
                .await
                .unwrap()
                .report
                .state,
            expected
        );
    }
    drop(guard);
    run.close().await.unwrap();
}

#[tokio::test]
async fn a_new_maintenance_claim_does_not_reopen_the_previous_completed_job() {
    let run = Experiment::create(&app(), MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    let guard = run.run.lock().await;
    let space = &guard.as_ref().unwrap().space;
    let c = Conversation {
        user: SELF_USER_ID,
        status: ConversationStatus::Completed,
        label: Some("maintenance".into()),
        ..Default::default()
    };
    let id = space
        .maintenance
        .conversations
        .add_conversation(ConversationRef::from(&c))
        .await
        .unwrap();
    let claim = space.maintenance.try_claim_processing().unwrap();
    assert!(space.maintenance.is_processing());
    assert_eq!(
        space
            .processing_report(ProcessingKind::Maintenance, id)
            .await
            .unwrap()
            .state,
        ProcessingState::Completed
    );
    drop(claim);
    drop(guard);
    run.close().await.unwrap();
}

#[derive(Debug)]
struct Blocked;
impl CompletionFeaturesDyn for Blocked {
    fn model_name(&self) -> String {
        "blocked-fixture".into()
    }
    fn completion(&self, _: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn close_cancels_owned_work_and_snapshot_refuses_an_active_writer() {
    let app = crate::testkit::app_state_core(
        "blocked",
        crate::testkit::models_with_completer(Blocked),
        vec![],
        "test-v1",
        0,
    );
    let run = Experiment::create(&app, MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    let result = run
        .observe(
            FormationInput {
                messages: vec![Message {
                    role: "user".into(),
                    content: vec!["remember this".to_string().into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
            Duration::from_millis(20),
        )
        .await
        .unwrap();
    assert!(result.timed_out);
    assert_eq!(result.report.state, ProcessingState::Running);
    assert!(run.snapshot().await.is_err());
    assert!(run.session_boundary().await.is_err());
    let space = run.run.lock().await.as_ref().unwrap().space.clone();
    tokio::time::timeout(Duration::from_secs(1), run.close())
        .await
        .unwrap()
        .unwrap();
    assert!(!space.is_processing());
    assert!(space.tasks.is_idle());
    assert!(space.engine.is_cancelled());
    assert!(
        run.wait(
            ProcessingKind::Formation,
            result.report.conversation,
            Duration::ZERO
        )
        .await
        .is_err()
    );
}

#[derive(Debug)]
struct NotifiedBlocked(Arc<tokio::sync::Notify>);
impl CompletionFeaturesDyn for NotifiedBlocked {
    fn model_name(&self) -> String {
        "notified-blocked-fixture".into()
    }
    fn completion(&self, _: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        self.0.notify_one();
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn close_interrupts_a_recall_holding_the_experiment_owner() {
    let started = Arc::new(tokio::sync::Notify::new());
    let app = crate::testkit::app_state_core(
        "blocked-recall",
        crate::testkit::models_with_completer(NotifiedBlocked(started.clone())),
        vec![],
        "test-v1",
        0,
    );
    let run = Arc::new(
        Experiment::create(&app, MemoryMode::Persistent, identity(), NOW)
            .await
            .unwrap(),
    );
    let pending = {
        let run = run.clone();
        tokio::spawn(async move {
            run.recall(RecallInput {
                budget: None,
                query: "recall".into(),
                ..Default::default()
            })
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), run.close())
        .await
        .unwrap()
        .unwrap();
    assert!(pending.await.unwrap().is_err());
}

#[tokio::test]
async fn queued_calls_cannot_dispatch_after_close_has_been_requested() {
    let captured = Arc::new(parking_lot::Mutex::new(vec![]));
    let app = crate::testkit::app_state_core(
        "queued-close",
        crate::testkit::models_with_completer(Recorder(captured.clone())),
        vec![],
        "test-v1",
        0,
    );
    let run = Experiment::create(&app, MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    let guard = run.run.lock().await;
    let mut queued = std::pin::pin!(run.recall(RecallInput {
        budget: None,
        query: "queued work".into(),
        ..Default::default()
    }));
    assert!(futures::poll!(queued.as_mut()).is_pending());
    let mut closing = std::pin::pin!(run.close());
    assert!(futures::poll!(closing.as_mut()).is_pending());
    drop(guard);
    let (result, closed) = tokio::join!(queued, closing);
    assert!(result.is_err());
    closed.unwrap();
    assert!(
        captured.lock().is_empty(),
        "closing must not dispatch queued model work"
    );
}

#[derive(Debug)]
struct RetryCompleter(std::sync::atomic::AtomicU64);
impl CompletionFeaturesDyn for RetryCompleter {
    fn model_name(&self) -> String {
        "retry-fixture".into()
    }
    fn completion(&self, _: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            Ok(AgentOutput {
                content: "finished".into(),
                failed_reason: first.then(|| "instrumented failure".into()),
                chat_history: vec![Message {
                    role: "assistant".into(),
                    content: vec!["finished".to_string().into()],
                    ..Default::default()
                }],
                usage: Usage {
                    input_tokens: if first { 11 } else { 13 },
                    output_tokens: 2,
                    requests: 1,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn formation_barrier_waits_for_retry_and_retains_both_cost_receipts() {
    let app = crate::testkit::app_state_core(
        "retry",
        crate::testkit::models_with_completer(RetryCompleter(std::sync::atomic::AtomicU64::new(0))),
        vec![],
        "test-v1",
        0,
    );
    let run = Experiment::create(&app, MemoryMode::Persistent, identity(), NOW)
        .await
        .unwrap();
    let result = run
        .observe(
            FormationInput {
                messages: vec![Message {
                    role: "user".into(),
                    content: vec!["remember the preference".to_string().into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
            Duration::from_secs(65),
        )
        .await
        .unwrap();
    assert!(!result.timed_out, "{result:?}");
    assert_eq!(result.report.state, ProcessingState::Completed);
    let costs = run.costs().await.unwrap();
    assert_eq!(costs.len(), 2, "{costs:?}");
    assert!(costs[0].failed);
    assert!(!costs[1].failed);
    assert_eq!(costs.iter().filter_map(|c| c.input_tokens).sum::<u64>(), 24);
    run.snapshot().await.unwrap();
    run.close().await.unwrap();
}
