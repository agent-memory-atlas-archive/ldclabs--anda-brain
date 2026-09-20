use super::*;
use crate::{
    consequence::*,
    runtime_api::{config::*, *},
};
use anda_cognitive_nexus::{governance::AuthContext, nexus::DEFAULT_SPACE};
use axum::{
    Router,
    body::{Body, to_bytes},
    routing::{get, post},
};
use http::{Request, StatusCode};
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const READER: &str = "kip:principal:r4-reader";
const OBSERVER: &str = "kip:principal:r4-observer";
const OTHER: &str = "kip:principal:r4-other";
const CONTROLLER: &str = "kip:principal:r4-controller";
const ST: &str = "ST-r4-reader-token";
const PROFILE: &str = "kip://profiles/cognitive-memory@2.1.0/";
fn auth(principal: &str) -> AuthContext {
    let mut a = AuthContext::principal(principal);
    a.auth_method = "test-authenticated-instrument".into();
    a
}
fn subject(n: u8) -> Principal {
    Principal::from_slice(&[n])
}
fn observer_contract() -> ObserverContract {
    ObserverContract {
        principal_id: OBSERVER.into(),
        configuration_digest: anda_cognitive_nexus::content_digest(
            &json!({"instrument":"r4-test"}),
        )
        .unwrap(),
        control_domain: "independent-test-instrument".into(),
        task_family: "memory.attention.v1".into(),
        metric: "delivery".into(),
        window: "durable_inbox_v1".into(),
        maximum_delay_ms: 3_600_000,
    }
}
fn config(name: &str, question: Option<String>) -> RuntimeConfig {
    RuntimeConfig {
        format: crate::runtime_api::FORMAT.into(),
        spaces: std::collections::BTreeMap::from([(
            name.into(),
            SpaceConfig {
                learning: None,
                utility: None,
                trust: None,
                semantic: None,
                bootstrap: true,
                audience: [READER.into(), OBSERVER.into(), OTHER.into()].into(),
                observers: vec![observer_contract()],
                subjects: vec![
                    ConfiguredSubject {
                        credential: ConfigCredential::CwtSubject {
                            subject: subject(10).to_string(),
                        },
                        principal: READER.into(),
                        observer: false,
                        audit_recipients: false,
                    },
                    ConfiguredSubject {
                        credential: ConfigCredential::CwtSubject {
                            subject: subject(11).to_string(),
                        },
                        principal: OBSERVER.into(),
                        observer: true,
                        audit_recipients: true,
                    },
                    ConfiguredSubject {
                        credential: ConfigCredential::CwtSubject {
                            subject: subject(12).to_string(),
                        },
                        principal: OTHER.into(),
                        observer: false,
                        audit_recipients: false,
                    },
                    ConfiguredSubject {
                        credential: ConfigCredential::SpaceTokenEnv {
                            variable: "R4_READER_TOKEN".into(),
                        },
                        principal: READER.into(),
                        observer: false,
                        audit_recipients: false,
                    },
                ],
                adapter: Some(InboxAdapter {
                    id: "attention_inbox_v1".into(),
                    controller_principal: CONTROLLER.into(),
                    recipient_principal: READER.into(),
                    message: "A memory needs your attention".into(),
                    question,
                    reply_timeout_ms: 30_000,
                    context: None,
                    limits: crate::action::ActionLimits {
                        recall: crate::recall_budget::RecallBudget {
                            max_tokens: 16000,
                            ..Default::default()
                        },
                        callbacks_ms: 2000,
                        lease_ms: 30000,
                        retry_ms: 10,
                        ..Default::default()
                    },
                }),
            },
        )]),
    }
}
fn app_for(
    name: &str,
    cfg: RuntimeConfig,
    keys: bool,
    store: Option<Arc<dyn object_store::ObjectStore>>,
) -> AppState {
    let key = signing_key(79);
    let template = app_state_core(
        name,
        Arc::new(Models::default()),
        if keys {
            vec![key.verifying_key()]
        } else {
            vec![]
        },
        "test",
        0,
    );
    let mut app = if let Some(store) = store {
        template.fork_with_store(store)
    } else {
        template
    };
    app.automatic = true;
    app.with_runtime_config(cfg, |v| (v == "R4_READER_TOKEN").then(|| ST.to_string()))
        .unwrap()
}
pub(crate) async fn fixture(
    name: &str,
    question: Option<String>,
) -> (AppState, Arc<Space>, String) {
    fixture_with(name, config(name, question), true, None).await
}
async fn fixture_with(
    name: &str,
    cfg: RuntimeConfig,
    keys: bool,
    store: Option<Arc<dyn object_store::ObjectStore>>,
) -> (AppState, Arc<Space>, String) {
    let app = app_for(name, cfg, keys, store);
    let space = create_loaded_space(&app, name).await;
    space
        .add_space_token(
            ST.into(),
            AddSpaceTokenInput {
                scope: TokenScope::All,
                name: "r4-reader".into(),
                expires_at: None,
                labels: None,
            },
            unix_ms(),
        )
        .await
        .unwrap();
    let target = created_ref(
        &space,
        r#"CREATE CONCEPT ?item {TYPE "Person" NAME "before"}"#,
        Default::default(),
    )
    .await;
    created_ref(&space,r#"MUTATE {CREATE CONCEPT ?p {TYPE "Preference" NAME "coordinate"} ENSURE PROPOSITION ?item (:target,"prefers",?p)}"#,kip::param("target",target.clone())).await;
    let watch=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"R4 retained task",status:"disarmed",condition:{element: :target}}}"#,kip::param("target",target.clone())).await;
    space.attention().arm_watch(watch.clone(), 1).await.unwrap();
    seed_kip(
        &space,
        kip::request_with(
            r#"UPDATE :id SET FIELDS {name:"after"}"#,
            kip::param("id", target),
        ),
    )
    .await;
    (app, space, watch)
}
fn router(app: AppState) -> Router {
    Router::new()
        .route(
            "/v1/{space_id}/attention",
            get(crate::handler::get_attention),
        )
        .route(
            "/v1/{space_id}/attention/{id}/responses",
            post(crate::handler::post_attention_response),
        )
        .route(
            "/v1/{space_id}/outcomes",
            post(crate::handler::post_outcome),
        )
        .route(
            "/v1/{space_id}/runtime/status",
            get(crate::handler::get_runtime_status),
        )
        .with_state(app)
}
pub(crate) fn token(space: &str, n: u8) -> String {
    signed_token(&signing_key(79), subject(n), space, "*")
}
async fn http(
    app: &AppState,
    name: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Json>,
) -> (StatusCode, Json) {
    let request = Request::builder()
        .method(method)
        .uri(format!("/v1/{name}/{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
        .unwrap();
    let response = router(app.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn delivered(space: &Space) -> AttentionItem {
    let runtime = space.memory_runtime().unwrap();
    let caller = runtime.authenticated_caller(auth(READER)).unwrap();
    for _ in 0..15 {
        let report = space.attention().tick().await.unwrap();
        let page = runtime
            .inbox(&caller, AttentionQuery::default())
            .await
            .unwrap();
        if let Some(item) = page
            .items
            .into_iter()
            .find(|i| i.delivery.is_some() && i.attempt_ref.is_some())
        {
            return item;
        }
        sleep(Duration::from_millis(15)).await;
        if report.error.is_some() {
            panic!("{report:?}");
        }
    }
    panic!(
        "delivery missing: {:?}",
        runtime
            .inbox(&caller, AttentionQuery::default())
            .await
            .unwrap()
    );
}
fn outcome(runtime: &MemoryRuntime, item: &AttentionItem, event: &str) -> OutcomeInput {
    OutcomeInput {
        utility: None,
        space_instance: runtime.scope().space_instance.clone(),
        attempt_ref: item.attempt_ref.clone().unwrap(),
        observer_configuration_digest: observer_contract().configuration_digest,
        event_key: event.into(),
        observed_at: anda_cognitive_nexus::time::now(),
        metric: "delivery".into(),
        window: "durable_inbox_v1".into(),
        observation: Observation::Measurement {
            terminal: true,
            outcome_status: OutcomeStatus::Success,
            magnitude: None,
            payload: json!({"delivery_digest":item.delivery.as_ref().unwrap()["request_digest"]}),
        },
        correction_of: None,
        safety_signal: None,
    }
}

#[tokio::test]
async fn r4_http_delivers_and_records_independent_outcome_without_learning() {
    let name = "r4_pipeline";
    let (app, space, _) = fixture(name, None).await;
    let item = delivered(&space).await;
    let (status, page) = http(&app, name, "GET", "attention", &token(name, 10), None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let runtime = space.memory_runtime().unwrap();
    let input = outcome(&runtime, &item, "event-1");
    let (status, receipt) = http(
        &app,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(input)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["result"]["native_committed"], true, "{receipt}");
    assert_eq!(receipt["result"]["learning_eligible"], false);
    let reference = receipt["result"]["outcome_ref"].as_str().unwrap();
    let view = crate::runtime_api::full_read(&runtime.nexus().session(auth(OBSERVER)), reference)
        .await
        .unwrap();
    assert_eq!(
        view["facets"][format!("{PROFILE}OutcomeRecord")]["attempt_ref"],
        input.attempt_ref
    );
    assert!(
        view["facets"][format!("{PROFILE}OutcomeRecord")]
            .get("payload")
            .is_none()
    );
    let (_, again) = http(
        &app,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(input)),
    )
    .await;
    assert_eq!(again["result"]["outcome_ref"], reference);
    for _ in 0..5 {
        space.attention().tick().await.unwrap();
        sleep(Duration::from_millis(15)).await;
    }
    let intents = space
        .memory
        .nexus()
        .system_session()
        .read_control(DEFAULT_SPACE, item.dispatch_ref.as_deref().unwrap(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intents.value["state"], "completed");
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_rejects_forged_observers_wrong_instances_future_and_conflicting_events() {
    let name = "r4_observer_guard";
    let (app, space, _) = fixture(name, None).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let input = outcome(&runtime, &item, "same-key");
    for credential in [
        ST.to_string(),
        token(name, 10),
        token(name, 12),
        String::new(),
    ] {
        let (status, body) = http(
            &app,
            name,
            "POST",
            "outcomes",
            &credential,
            Some(json!(input)),
        )
        .await;
        assert!(
            matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
            "{status} {body}"
        );
    }
    let mut wrong = input.clone();
    wrong.space_instance = "another-instance".into();
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(wrong))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut future = input.clone();
    future.observed_at = kip::timestamp(unix_ms() + 100000);
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(future))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(input))
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut conflict = input.clone();
    conflict.safety_signal = Some("severe independent safety report".into());
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(conflict))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let audit = runtime
        .consequences()
        .receipts(auth(OBSERVER), ObservationLane::Action, 0, 100)
        .await
        .unwrap();
    assert!(
        audit
            .items
            .iter()
            .any(|r| r.status == "conflict_audit" && r.safety_pending)
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_inbox_authentication_recipient_filter_and_opaque_cursor_are_read_only() {
    let name = "r4_inbox_read";
    let (app, space, _) = fixture(name, None).await;
    delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let before = space
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let (status, _) = http(&app, name, "GET", "attention", "", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, other) = http(&app, name, "GET", "attention", &token(name, 12), None).await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(other["result"]["items"], json!([]));
    let (status, space_token) = http(&app, name, "GET", "attention", ST, None).await;
    assert_eq!(status, StatusCode::OK, "{space_token}");
    let caller = runtime.authenticated_caller(auth(READER)).unwrap();
    let page = runtime
        .inbox(
            &caller,
            AttentionQuery {
                limit: Some(1),
                cursor: None,
            },
        )
        .await
        .unwrap();
    let cursor = page.next_cursor.unwrap();
    assert!(!cursor.contains("wake"));
    assert!(
        runtime
            .inbox(
                &runtime.authenticated_caller(auth(OTHER)).unwrap(),
                AttentionQuery {
                    cursor: Some(cursor.clone()),
                    limit: Some(1)
                }
            )
            .await
            .is_err()
    );
    let mut bad = cursor.into_bytes();
    bad[8] = if bad[8] == b'A' { b'B' } else { b'A' };
    assert!(
        runtime
            .inbox(
                &caller,
                AttentionQuery {
                    cursor: Some(String::from_utf8(bad).unwrap()),
                    limit: Some(1)
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        before,
        space
            .memory
            .nexus()
            .store
            .get_space(DEFAULT_SPACE)
            .await
            .unwrap()
            .seq
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_disabled_cwt_verifier_does_not_turn_local_fallback_into_an_observer() {
    let name = "r4_no_signatures";
    let (app, space, _) = fixture_with(name, config(name, None), false, None).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let input = outcome(&runtime, &item, "not-authorized");
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(input))
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http(&app, name, "POST", "outcomes", ST, Some(json!(input)))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, value) = http(&app, name, "GET", "runtime/status", ST, None).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["result"]["observation_enabled"], false);
    assert_eq!(value["result"]["configured"], true);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_answering_an_unsent_question_suppresses_delivery_and_statements_are_not_outcomes() {
    let name = "r4_response";
    let (app, space, _) = fixture(name, Some("Which date?".into())).await;
    space.attention().tick().await.unwrap();
    let runtime = space.memory_runtime().unwrap();
    let caller = runtime.authenticated_caller(auth(READER)).unwrap();
    let parent = runtime
        .inbox(&caller, AttentionQuery::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.clarification.is_some())
        .unwrap();
    let path = format!("attention/{}/responses", parent.id);
    let response = json!({"kind":"clarification","event_key":"answer-1","answer":"Tomorrow"});
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            &path,
            &token(name, 12),
            Some(response.clone())
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, receipt) = http(
        &app,
        name,
        "POST",
        &path,
        &token(name, 10),
        Some(response.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(
        receipt["result"]["status"],
        "answer_received_not_authorization"
    );
    assert_eq!(
        http(&app, name, "POST", &path, ST, Some(response.clone()))
            .await
            .0,
        StatusCode::OK
    );
    for _ in 0..12 {
        space.attention().tick().await.unwrap();
        sleep(Duration::from_millis(15)).await;
    }
    let page = runtime
        .inbox(&caller, AttentionQuery::default())
        .await
        .unwrap();
    assert!(!page.items.iter().any(|i| {
        i.delivery
            .as_ref()
            .is_some_and(|d| d["payload"]["question"].is_string())
    }));
    let statement = json!({"kind":"agent_statement","event_key":"report-1","statement":"The agent says its task succeeded"});
    let (status, receipt) = http(&app, name, "POST", &path, ST, Some(statement.clone())).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let reference = receipt["result"]["evidence_ref"].as_str().unwrap();
    let record = crate::runtime_api::full_read(&runtime.nexus().session(auth(READER)), reference)
        .await
        .unwrap();
    assert!(
        record["facets"]
            .get(format!("{PROFILE}OutcomeRecord"))
            .is_none()
    );
    let (_, replayed) = http(&app, name, "POST", &path, ST, Some(statement)).await;
    assert_eq!(replayed["result"]["evidence_ref"], reference);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_progress_and_corrections_preserve_original_outcomes_and_unknown_costs() {
    let name = "r4_progress";
    let (app, space, _) = fixture(name, None).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let mut input = outcome(&runtime, &item, "progress");
    input.observation = Observation::Measurement {
        terminal: false,
        outcome_status: OutcomeStatus::Unknown,
        magnitude: None,
        payload: json!({"elapsed_ms":null}),
    };
    let (status, progress) = http(
        &app,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(input)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{progress}");
    assert_eq!(progress["result"]["outcome_status"], "unknown");
    let intent = runtime
        .nexus()
        .system_session()
        .read_control(DEFAULT_SPACE, item.dispatch_ref.as_deref().unwrap(), None)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(intent.value["state"], "completed");
    let terminal = outcome(&runtime, &item, "terminal");
    let (_, first) = http(
        &app,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(terminal)),
    )
    .await;
    let first = first["result"]["outcome_ref"].as_str().unwrap().to_string();
    let mut correction = outcome(&runtime, &item, "correction");
    correction.correction_of = Some("terminal".into());
    correction.safety_signal = Some("independent safety discrepancy".into());
    correction.observation = Observation::Measurement {
        terminal: true,
        outcome_status: OutcomeStatus::Failure,
        magnitude: None,
        payload: json!({"correction_reason":"instrument review"}),
    };
    let (status, corrected) = http(
        &app,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(correction)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corrected}");
    assert_eq!(corrected["result"]["status"], "correction_recorded");
    assert_eq!(corrected["result"]["safety_pending"], true);
    assert_ne!(corrected["result"]["outcome_ref"], first);
    let original = crate::runtime_api::full_read(&runtime.nexus().session(auth(OBSERVER)), &first)
        .await
        .unwrap();
    assert_eq!(
        original["facets"][format!("{PROFILE}OutcomeRecord")]["outcome_status"],
        "success"
    );
    let intent = runtime
        .nexus()
        .system_session()
        .read_control(DEFAULT_SPACE, item.dispatch_ref.as_deref().unwrap(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intent.value["outcome_ref"], first);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_late_observations_are_auditable_without_creating_native_success() {
    let name = "r4_late";
    let mut cfg = config(name, None);
    cfg.spaces.get_mut(name).unwrap().observers[0].maximum_delay_ms = 1;
    let (app, space, _) = fixture_with(name, cfg, true, None).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let mut input = outcome(&runtime, &item, "late");
    input.safety_signal = Some("late severe outcome requires review".into());
    let (status, receipt) = http(
        &app,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(input)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["result"]["status"], "late_audit");
    assert_eq!(receipt["result"]["native_committed"], false);
    let audit = runtime
        .consequences()
        .receipts(auth(OBSERVER), ObservationLane::Action, 0, 100)
        .await
        .unwrap();
    assert!(
        audit
            .items
            .iter()
            .any(|r| r.status == "late_audit" && r.safety_pending)
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_bootstrap_does_not_restore_revoked_observer_grants_after_restart() {
    let name = "r4_revocation";
    let (app, space, _) = fixture(name, None).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let input = outcome(&runtime, &item, "after-revocation");
    let nexus = runtime.nexus();
    let grant = nexus
        .governance()
        .grants_for(DEFAULT_SPACE, OBSERVER, &[])
        .await
        .unwrap()
        .into_iter()
        .find(|g| g.actions.contains(&"record_outcome".into()))
        .unwrap();
    nexus
        .system_session()
        .revoke_grant(DEFAULT_SPACE, grant._id)
        .await
        .unwrap();
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(input))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    space.close().await.unwrap();
    let restarted = app_for(name, config(name, None), true, Some(app.object_store()));
    assert_eq!(
        http(
            &restarted,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(input))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    restarted
        .load_space_with(name, false, false)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn r4_json_cbor_and_markdown_negotiation_remains_compatible() {
    let name = "r4_formats";
    let (app, space, _) = fixture(name, None).await;
    let item = delivered(&space).await;
    let input = outcome(&space.memory_runtime().unwrap(), &item, "cbor-event");
    let body = cbor2::to_vec(&input).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/{name}/outcomes"))
        .header("Authorization", format!("Bearer {}", token(name, 11)))
        .header("Content-Type", "application/cbor")
        .header("Accept", "text/markdown")
        .body(Body::from(body))
        .unwrap();
    let response = router(app.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("markdown")
    );
    let body = to_bytes(response.into_body(), 262144).await.unwrap();
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .contains("native_committed")
    );
    let request = Request::builder()
        .uri(format!("/v1/{name}/attention"))
        .header("Authorization", format!("Bearer {ST}"))
        .header("Accept", "application/cbor")
        .body(Body::empty())
        .unwrap();
    let response = router(app.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 262144).await.unwrap();
    let value: Json = cbor2::from_slice(&body).unwrap();
    assert!(value["result"]["items"].is_array());
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_observation_ack_loss_recovers_after_cold_restart_without_duplicate_evidence() {
    use anda_object_store::fault::{FaultOp, FaultRule, FaultStore};
    let name = "r4_ack";
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let (app, space, _) = fixture_with(name, config(name, None), true, Some(Arc::new(store))).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let input = outcome(&runtime, &item, "ack-lost");
    faults.push_rule(FaultRule {
        skip: 1,
        ..FaultRule::fail_once(FaultOp::Put, "/runtime-api/observations/")
    });
    let (status, body) = http(
        &app,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(input)),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    faults.reset();
    space.close().await.unwrap();
    let restarted = app_for(name, config(name, None), true, Some(app.object_store()));
    let (status, receipt) = http(
        &restarted,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(input)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["result"]["native_committed"], true);
    let space = restarted.load_space_with(name, false, false).await.unwrap();
    let response = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?e.id) WHERE {?e EVIDENCE {evidence_class:"outcome"}} LIMIT 100"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        kip::ok_result(&response).unwrap().as_array().unwrap().len(),
        1
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn r4_owned_outcome_write_survives_cancelled_waiter_and_close_drains_native_put() {
    use anda_object_store::fault::{FaultGate, FaultKind, FaultOp, FaultRule, FaultStore};
    let name = "r4_drain";
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let (app, space, _) = fixture_with(name, config(name, None), true, Some(Arc::new(store))).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let input = outcome(&runtime, &item, "owned-write");
    let gate = FaultGate::new();
    faults.push_rule(FaultRule {
        kind: FaultKind::PauseAfter(gate.clone()),
        ..FaultRule::fail_once(FaultOp::Put, "/evidence/")
    });
    let waiter = {
        let runtime = runtime.consequences();
        let input = input.clone();
        tokio::spawn(async move { runtime.submit(auth(OBSERVER), input).await })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_entered())
        .await
        .unwrap();
    waiter.abort();
    let _ = waiter.await;
    assert!(space.is_busy());
    let closing = {
        let space = space.clone();
        tokio::spawn(async move { space.close().await })
    };
    sleep(Duration::from_millis(20)).await;
    assert!(!closing.is_finished());
    gate.release();
    closing.await.unwrap().unwrap();
    faults.reset();
    let restarted = app_for(name, config(name, None), true, Some(app.object_store()));
    let (status, receipt) = http(
        &restarted,
        name,
        "POST",
        "outcomes",
        &token(name, 11),
        Some(json!(input)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["result"]["native_committed"], true);
    restarted
        .load_space_with(name, false, false)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn r4_delivery_proof_and_actual_attempt_author_reject_fabricated_success() {
    let name = "r4_provenance";
    let (app, space, _) = fixture(name, None).await;
    let item = delivered(&space).await;
    let runtime = space.memory_runtime().unwrap();
    let mut input = outcome(&runtime, &item, "bad-proof");
    if let Observation::Measurement { payload, .. } = &mut input.observation {
        *payload = json!({"delivery_digest":"invented"});
    }
    assert_eq!(
        http(
            &app,
            name,
            "POST",
            "outcomes",
            &token(name, 11),
            Some(json!(input))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let input = outcome(&runtime, &item, "self-observation");
    let nexus = runtime.nexus();
    nexus
        .system_session()
        .create_grant(
            DEFAULT_SPACE,
            anda_cognitive_nexus::governance::store::GrantDraft {
                grantee_principal: CONTROLLER.into(),
                actions: vec!["record_outcome".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut contract = observer_contract();
    contract.principal_id = CONTROLLER.into();
    let ingress = ConsequenceRuntime::new(
        nexus,
        crate::attention::Directory::new(app.object_store(), 0),
        runtime.scope().clone(),
        vec![contract],
        false,
        #[cfg(feature = "learning")]
        None,
    );
    assert!(matches!(
        ingress.submit(auth(CONTROLLER), input).await,
        Err(RuntimeError::Forbidden)
    ));
    ingress.shutdown().await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn r7_semantic_fire_uses_existing_inbox_gate_and_hides_global_counts_from_readers() {
    use crate::attention::semantic::*;
    struct Evaluator;
    #[async_trait::async_trait]
    impl SemanticEvaluator for Evaluator {
        async fn evaluate(&self, request: Json) -> Result<String, BoxError> {
            let input: Json =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap())?;
            Ok(json!({"ticket_ref":input["ticket_ref"],"page_digest":input["page_digest"],"condition_digest":input["condition_digest"],"evaluator":input["evaluator"],
                "judgments":input["candidates"].as_array().unwrap().iter().map(|c|json!({"candidate_id":c["id"],"result":"match","rationale":"Fixture after.name is the configured reply marker"})).collect::<Vec<_>>()}).to_string())
        }
    }
    let name = "r7_inbox";
    let mut bindings = config(name, None)
        .resolve(|v| (v == "R4_READER_TOKEN").then(|| ST.into()))
        .unwrap();
    bindings.spaces.get_mut(name).unwrap().semantic = Some(SemanticBindings {
        config: SemanticConfig {
            version: "inbox-semantic/1".into(),
            principal: CONTROLLER.into(),
            model: "fixture-1".into(),
            endpoint: "https://example.invalid/v1/chat/completions".into(),
            automatic: false,
            limits: Default::default(),
        },
        evaluator: Arc::new(Evaluator),
    });
    let mut app = app_state_core(name, Arc::new(Models::default()), vec![], "test", 0);
    app.automatic = true;
    let app = app.with_memory_runtime_bindings(bindings).unwrap();
    let space = create_loaded_space(&app, name).await;
    let target = created_ref(
        &space,
        r#"CREATE CONCEPT ?item {TYPE "Person" NAME "before"}"#,
        Default::default(),
    )
    .await;
    created_ref(&space,r#"MUTATE {CREATE CONCEPT ?p {TYPE "Preference" NAME "coordinate"} ENSURE PROPOSITION ?item (:target,"prefers",?p)}"#,kip::param("target",target.clone())).await;
    let watch=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"R7 inbox",status:"disarmed",condition:{element: :target,text:"reply marker"}}}"#,kip::param("target",target.clone())).await;
    space.attention().arm_watch(watch, 1).await.unwrap();
    seed_kip(
        &space,
        kip::request_with(
            r#"UPDATE :target SET FIELDS {name:"reply marker"}"#,
            kip::param("target", target),
        ),
    )
    .await;
    let pass = space
        .attention()
        .semantic()
        .unwrap()
        .run_once()
        .await
        .unwrap();
    assert_eq!(pass.fired, 1, "{pass:?}");
    let item = delivered(&space).await;
    assert!(item.decision_ref.is_some());
    assert!(item.attempt_ref.is_some());
    let runtime = space.memory_runtime().unwrap();
    let reader = runtime.authenticated_caller(auth(READER)).unwrap();
    let auditor = runtime.authenticated_caller(auth(OBSERVER)).unwrap();
    let reader = runtime.status(&reader, false).await.unwrap();
    let auditor = runtime.status(&auditor, true).await.unwrap();
    assert!(reader.semantic_attention.configured);
    assert!(reader.semantic_attention.last_pass.is_none());
    assert_eq!(auditor.semantic_attention.last_pass.unwrap().fired, 1);
    space.close().await.unwrap();
}
