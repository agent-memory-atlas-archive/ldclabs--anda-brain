use super::*;
mod http;
use crate::attention::semantic::*;
use crate::runtime_api::{MemoryRuntimeBindings, SpaceRuntimeBindings};
use anda_cognitive_nexus::{
    attention::{AttentionConfig, RuntimePin},
    governance::AuthContext,
};
use async_trait::async_trait;
use futures::TryStreamExt;
use object_store::ObjectStore;
use std::sync::{Mutex, atomic::AtomicUsize};

#[derive(Clone, Default)]
struct EvalGate {
    entered: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::Notify>,
}
impl EvalGate {
    fn new() -> Self {
        Self::default()
    }
    async fn wait_entered(&self) {
        self.entered.notified().await;
    }
    async fn pause(&self) {
        self.entered.notify_one();
        self.released.notified().await;
    }
    fn release(&self) {
        self.released.notify_one();
    }
}
const HOST: &str = "kip:principal:r7-controller";
#[derive(Clone, Copy)]
enum Mode {
    Match,
    NoMatch,
    NamedMatch,
    OneUnknown,
    Omit,
    Complete,
    WrongPin,
    Truncate,
}
struct Evaluator {
    mode: Mutex<Mode>,
    calls: AtomicUsize,
    requests: Mutex<Vec<Value>>,
    gate: Option<EvalGate>,
}
impl Evaluator {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(mode),
            calls: AtomicUsize::new(0),
            requests: Default::default(),
            gate: None,
        })
    }
}
fn reply(request: &Value, mode: Mode) -> String {
    let material: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let mut rows:Vec<_> = material["candidates"].as_array().unwrap().iter().enumerate().map(|(i,c)|json!({
        "candidate_id":c["id"],"result":match mode {Mode::Match=>"match",Mode::NamedMatch if c["after"]["name"] == "actual semantic reply"=>"match",Mode::OneUnknown if i==0=>"unknown",_=>"no_match"},
        "rationale":format!("Fixture event field after.name = {}; before.name = {}. This fixture is mechanism evidence only.", c["after"]["name"],c["before"]["name"])
    })).collect();
    if matches!(mode, Mode::Omit) {
        rows.pop();
    }
    let mut response = json!({"ticket_ref":material["ticket_ref"],"page_digest":material["page_digest"],
        "condition_digest":material["condition_digest"],"evaluator":material["evaluator"],"judgments":rows});
    if matches!(mode, Mode::Complete) {
        response["complete"] = json!(true);
    }
    if matches!(mode, Mode::WrongPin) {
        response["evaluator"]["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    }
    if matches!(mode, Mode::Truncate) {
        return response.to_string()[..30].to_string();
    }
    response.to_string()
}
#[async_trait]
impl SemanticEvaluator for Evaluator {
    async fn evaluate(&self, request: Value) -> Result<String, BoxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request.clone());
        if let Some(gate) = &self.gate {
            gate.pause().await;
        }
        Ok(reply(&request, *self.mode.lock().unwrap()))
    }
}
fn contract() -> SemanticConfig {
    SemanticConfig {
        version: "r7-mechanism-fixture/1".into(),
        principal: HOST.into(),
        model: "fixture-pinned-v1".into(),
        endpoint: "http://127.0.0.1:1/v1/chat/completions".into(),
        automatic: false,
        limits: SemanticLimits {
            changes_per_page: 200,
            retry_ms: 1,
            ..Default::default()
        },
    }
}
fn semantic_app(
    name: &str,
    store: Arc<dyn object_store::ObjectStore>,
    config: SemanticConfig,
    evaluator: Arc<dyn SemanticEvaluator>,
) -> AppState {
    let mut app = runtime_app(store, fast_policy());
    app.automatic = true;
    app.with_memory_runtime_bindings(MemoryRuntimeBindings {
        spaces: std::collections::BTreeMap::from([(
            name.into(),
            SpaceRuntimeBindings {
                pin: RuntimePin {
                    id: "r7-test-host".into(),
                    digest: anda_cognitive_nexus::content_digest(&json!("r7-test-host")).unwrap(),
                },
                subjects: vec![],
                observers: vec![],
                actions: None,
                inbox: None,
                utility: None,
                trust: None,
                semantic: Some(SemanticBindings { config, evaluator }),
                #[cfg(feature = "learning")]
                learning: None,
                bootstrap: true,
                audience: Default::default(),
                inbox_recipient: None,
            },
        )]),
    })
    .unwrap()
}
async fn fixture(
    name: &str,
    evaluator: Arc<Evaluator>,
    config: SemanticConfig,
) -> (AppState, Arc<Space>, String, String) {
    let app = semantic_app(name, Arc::new(InMemory::new()), config, evaluator);
    let space = create_loaded_space(&app, name).await;
    let target = target(&space).await;
    let watch = new_watch(&space, &target, "delta", None, true).await;
    change(&space, &target, "actual semantic reply").await;
    (app, space, target, watch)
}
async fn change(space: &Space, target: &str, name: &str) {
    let mut p = kip::param("target", target);
    p.insert("name".into(), json!(name));
    seed_kip(
        space,
        kip::request_with("UPDATE :target SET FIELDS {name: :name}", p),
    )
    .await;
}
fn session(space: &Space) -> anda_cognitive_nexus::nexus::Session {
    let mut auth = AuthContext::principal(HOST);
    auth.auth_method = "brain:registered-semantic-controller".into();
    space.memory.nexus().session(auth)
}

#[tokio::test]
async fn r7_mixed_watch_evaluates_exact_history_and_native_commit_fires_once() {
    let evaluator = Evaluator::new(Mode::NoMatch);
    let (_app, space, target, watch) = fixture("r7_mixed", evaluator.clone(), contract()).await;
    let runtime = space.attention().semantic().unwrap();
    let pass = runtime.run_once().await.unwrap();
    assert_eq!(pass.advanced, 1, "{pass:?}");
    assert_eq!(pass.fired, 0);
    assert_eq!(watch_state(&space, &watch).await[0], "armed");
    let material: Value = serde_json::from_str(
        evaluator.requests.lock().unwrap()[0]["messages"][1]["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(material["condition"]["element"], target);
    assert_eq!(material["condition"]["text"], "a verified reply");
    assert_eq!(material["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(material["candidates"][0]["before"]["name"], "before");
    assert_eq!(
        material["candidates"][0]["after"]["name"],
        "actual semantic reply"
    );
    *evaluator.mode.lock().unwrap() = Mode::Match;
    change(&space, &target, "later matching reply").await;
    let pass = runtime.run_once().await.unwrap();
    assert_eq!(pass.fired, 1, "{pass:?}");
    assert_eq!(wake_count(&space).await, 1);
    let progress = runtime.progress(&watch).await.unwrap().unwrap();
    let record = session(&space)
        .read_control(
            DEFAULT_SPACE,
            progress.evaluation_ref.as_deref().unwrap(),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.value["accepted"], true);
    let pin = serde_json::from_value(record.value["material"].clone()).unwrap();
    let material = session(&space)
        .read_artifact(DEFAULT_SPACE, &pin)
        .await
        .unwrap();
    assert!(material.to_string().contains("later matching reply"));
    runtime.run_once().await.unwrap();
    assert_eq!(wake_count(&space).await, 1);
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 2);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r7_unknown_omitted_forged_or_truncated_judgment_never_advances_silence() {
    for (index, mode) in [
        Mode::OneUnknown,
        Mode::Omit,
        Mode::Complete,
        Mode::WrongPin,
        Mode::Truncate,
    ]
    .into_iter()
    .enumerate()
    {
        let evaluator = Evaluator::new(mode);
        let name = format!("r7_invalid_{index}");
        let app = semantic_app(
            &name,
            Arc::new(InMemory::new()),
            contract(),
            evaluator.clone(),
        );
        let space = create_loaded_space(&app, &name).await;
        let target = target(&space).await;
        let watch = new_watch(&space, &target, "silence", Some(unix_ms() + 150), true).await;
        change(&space, &target, "first reply").await;
        change(&space, &target, "second reply").await;
        sleep(Duration::from_millis(170)).await;
        let before = watch_state(&space, &watch).await;
        let runtime = space.attention().semantic().unwrap();
        let pass = runtime.run_once().await.unwrap();
        assert_eq!(pass.deferred, 1, "{pass:?}");
        assert_eq!(before, watch_state(&space, &watch).await);
        assert_eq!(wake_count(&space).await, 0);
        let p = runtime.progress(&watch).await.unwrap().unwrap();
        assert_eq!(p.status.as_deref(), Some("deferred"));
        assert!(p.evaluation_ref.is_some());
        let second = runtime.run_once().await.unwrap();
        assert_eq!(second.advanced, 0);
        runtime.run_once().await.unwrap();
        assert!(evaluator.calls.load(Ordering::SeqCst) <= 2);
        // An explicit operator retry keeps the same pinned page and history.
        *evaluator.mode.lock().unwrap() = Mode::NoMatch;
        runtime.retry(watch.clone()).await.unwrap();
        let pass = runtime.run_once().await.unwrap();
        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(
            runtime.progress(&watch).await.unwrap().unwrap().ticket_ref,
            p.ticket_ref
        );
        space.close().await.unwrap();
    }
}

#[tokio::test]
async fn r7_structurally_excluded_empty_page_proves_silence_without_model() {
    let evaluator = Evaluator::new(Mode::Match);
    let app = semantic_app(
        "r7_empty",
        Arc::new(InMemory::new()),
        contract(),
        evaluator.clone(),
    );
    let space = create_loaded_space(&app, "r7_empty").await;
    let a = target(&space).await;
    let b = target(&space).await;
    let watch = new_watch(&space, &a, "silence", Some(unix_ms() + 80), true).await;
    change(&space, &b, "different source cannot satisfy full selector").await;
    sleep(Duration::from_millis(100)).await;
    let runtime = space.attention().semantic().unwrap();
    let pass = runtime.run_once().await.unwrap();
    assert_eq!(pass.fired, 1, "{pass:?}");
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 0);
    let p = runtime.progress(&watch).await.unwrap().unwrap();
    let page = session(&space)
        .read_prepared_watch_page(DEFAULT_SPACE, p.ticket_ref.as_deref().unwrap())
        .await
        .unwrap();
    assert!(page.candidates.is_empty());
    assert!(page.deadline_covered);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r7_model_wait_releases_native_and_attention_locks_rearm_rejects_old_result() {
    let gate = EvalGate::new();
    let evaluator = Arc::new(Evaluator {
        mode: Mutex::new(Mode::Match),
        calls: AtomicUsize::new(0),
        requests: Default::default(),
        gate: Some(gate.clone()),
    });
    let (_app, space, _, watch) = fixture("r7_rearm", evaluator, contract()).await;
    let runtime = space.attention().semantic().unwrap();
    let task = {
        let r = runtime.clone();
        tokio::spawn(async move { r.run_once().await })
    };
    tokio::time::timeout(Duration::from_secs(3), gate.wait_entered())
        .await
        .unwrap();
    // This would deadlock if the callback held either native or attention lock.
    tokio::time::timeout(
        Duration::from_secs(2),
        space.attention().arm_watch(watch.clone(), 2),
    )
    .await
    .unwrap()
    .unwrap();
    gate.release();
    let pass = task.await.unwrap().unwrap();
    assert_eq!(pass.fired, 0);
    assert_eq!(watch_state(&space, &watch).await[1]["arm_generation"], 2);
    assert_eq!(wake_count(&space).await, 0);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r7_revocation_or_purge_while_model_runs_rejects_material_and_commit() {
    for erase in [false, true] {
        let gate = EvalGate::new();
        let evaluator = Arc::new(Evaluator {
            mode: Mutex::new(Mode::Match),
            calls: AtomicUsize::new(0),
            requests: Default::default(),
            gate: Some(gate.clone()),
        });
        let name = if erase { "r7_purge" } else { "r7_revoke" };
        let (_app, space, target, watch) = fixture(name, evaluator, contract()).await;
        let before = watch_state(&space, &watch).await;
        let runtime = space.attention().semantic().unwrap();
        let task = {
            let r = runtime.clone();
            tokio::spawn(async move { r.run_once().await })
        };
        tokio::time::timeout(Duration::from_secs(3), gate.wait_entered())
            .await
            .unwrap();
        if erase {
            seed_kip(
                &space,
                kip::request_with("PURGE :id CONFIRM \"PURGE\"", kip::param("id", target)),
            )
            .await;
        } else {
            space
                .memory
                .nexus()
                .governance()
                .set_principal_status(HOST, "revoked", "kip:principal:system")
                .await
                .unwrap();
        }
        gate.release();
        let result = task.await.unwrap();
        if let Ok(pass) = result {
            assert_eq!(pass.advanced, 0);
        }
        assert_eq!(watch_state(&space, &watch).await, before);
        assert_eq!(wake_count(&space).await, 0);
        space.close().await.unwrap();
    }
}

#[tokio::test]
async fn r7_timeout_and_input_budget_do_not_claim_coverage_or_spend_unbounded_calls() {
    for timeout in [true, false] {
        let gate = EvalGate::new();
        let evaluator = Arc::new(Evaluator {
            mode: Mutex::new(Mode::Match),
            calls: AtomicUsize::new(0),
            requests: Default::default(),
            gate: timeout.then_some(gate),
        });
        let mut cfg = contract();
        if timeout {
            cfg.limits.callback_ms = 20;
        } else {
            cfg.limits.input_tokens = 512;
        }
        let name = if timeout { "r7_timeout" } else { "r7_budget" };
        let (_app, space, _, watch) = fixture(name, evaluator.clone(), cfg).await;
        let before = watch_state(&space, &watch).await;
        let runtime = space.attention().semantic().unwrap();
        let pass = runtime.run_once().await.unwrap();
        assert_eq!(pass.advanced, 0);
        assert_eq!(watch_state(&space, &watch).await, before);
        assert_eq!(evaluator.calls.load(Ordering::SeqCst), usize::from(timeout));
        assert!(
            pass.reason
                .unwrap()
                .contains(if timeout { "timeout" } else { "budget" })
        );
        space.close().await.unwrap();
    }
}

#[tokio::test]
async fn r7_committed_page_recovers_after_checkpoint_failure_and_cold_reopen() {
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let store = Arc::new(store);
    let evaluator = Evaluator::new(Mode::Match);
    let config = contract();
    let app = semantic_app(
        "r7_recovery",
        store.clone(),
        config.clone(),
        evaluator.clone(),
    );
    let space = create_loaded_space(&app, "r7_recovery").await;
    let target = target(&space).await;
    let watch = new_watch(&space, &target, "delta", None, true).await;
    change(&space, &target, "private replay material").await;
    // Job writes: new intent, prepared ref, spend reservation, response Artifact,
    // then native commit succeeded but saving the accepted result fails.
    faults.push_rule(FaultRule {
        skip: 4,
        ..FaultRule::fail_once(FaultOp::Put, format!("/jobs/{watch}"))
    });
    let runtime = space.attention().semantic().unwrap();
    assert!(runtime.run_once().await.is_err());
    assert_eq!(watch_state(&space, &watch).await[0], "fired");
    assert_eq!(wake_count(&space).await, 1);
    faults.reset();
    drop(runtime);
    drop(space);
    evict_all(&app).await;
    let restarted = semantic_app("r7_recovery", store.clone(), config, evaluator.clone());
    let loaded = restarted
        .load_space_with("r7_recovery", false, false)
        .await
        .unwrap();
    let before = loaded
        .memory
        .nexus()
        .store
        .get_space(DEFAULT_SPACE)
        .await
        .unwrap()
        .seq;
    let pass = loaded
        .attention()
        .semantic()
        .unwrap()
        .run_once()
        .await
        .unwrap();
    assert_eq!(pass.fired, 1, "{pass:?}");
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 1);
    assert_eq!(wake_count(&loaded).await, 1);
    assert_eq!(
        loaded
            .memory
            .nexus()
            .store
            .get_space(DEFAULT_SPACE)
            .await
            .unwrap()
            .seq,
        before
    );
    // The off-graph journal has only references and counters, no response/page text.
    let mut objects = store.list(None);
    while let Some(meta) = objects.try_next().await.unwrap() {
        if meta.location.as_ref().contains("/semantic/") {
            let bytes = store
                .get(&meta.location)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(!text.contains("private replay material"));
            assert!(!text.contains("Fixture event"));
        }
    }
    loaded.close().await.unwrap();
}

#[tokio::test]
async fn r7_evaluator_change_cannot_reuse_an_old_arm_or_old_coverage() {
    let evaluator = Evaluator::new(Mode::Match);
    let (_app, space, _, watch) = fixture("r7_pin", evaluator.clone(), contract()).await;
    let before = watch_state(&space, &watch).await;
    let system = space.memory.nexus().system_session();
    let saved = system
        .read_control(DEFAULT_SPACE, "attention/config", None)
        .await
        .unwrap()
        .unwrap();
    let mut config: AttentionConfig = serde_json::from_value(saved.value).unwrap();
    config.pins.evaluator = Some(RuntimePin {
        id: "other-model".into(),
        digest: anda_cognitive_nexus::content_digest(&json!("other")).unwrap(),
    });
    system
        .set_attention_config(DEFAULT_SPACE, saved.version, config)
        .await
        .unwrap();
    assert!(
        space
            .attention()
            .semantic()
            .unwrap()
            .run_once()
            .await
            .is_err()
    );
    assert_eq!(watch_state(&space, &watch).await, before);
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 0);
    let (version, _) = space.attention().configuration().await.unwrap().unwrap();
    space
        .attention()
        .reconfigure_evaluator(version)
        .await
        .unwrap();
    assert_eq!(watch_state(&space, &watch).await, before);
    let pass = space
        .attention()
        .semantic()
        .unwrap()
        .run_once()
        .await
        .unwrap();
    assert_eq!(
        pass.advanced, 0,
        "old arm cannot inherit a replacement basis"
    );
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 0);
    // Explicit re-arm begins a new interval; the previous reply is not replayed
    // into it. Only a new change may fire it.
    space.attention().arm_watch(watch.clone(), 2).await.unwrap();
    space.close().await.unwrap();
}

#[tokio::test]
async fn r7_text_only_condition_judges_each_authorized_transition() {
    let evaluator = Evaluator::new(Mode::NamedMatch);
    let app = semantic_app(
        "r7_text_only",
        Arc::new(InMemory::new()),
        contract(),
        evaluator.clone(),
    );
    let space = create_loaded_space(&app, "r7_text_only").await;
    let target = target(&space).await;
    let watch=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"R7 text only",status:"disarmed",condition:{text:"an event sets the exact name actual semantic reply"}}}"#,Default::default()).await;
    space.attention().arm_watch(watch.clone(), 1).await.unwrap();
    change(&space, &target, "an unrelated update").await;
    let runtime = space.attention().semantic().unwrap();
    let pass = runtime.run_once().await.unwrap();
    assert_eq!(pass.advanced, 1, "{pass:?}");
    assert_eq!(pass.fired, 0);
    change(&space, &target, "actual semantic reply").await;
    let pass = runtime.run_once().await.unwrap();
    assert_eq!(pass.fired, 1, "{pass:?}");
    {
        let requests = evaluator.requests.lock().unwrap();
        let material: Value =
            serde_json::from_str(requests[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(material["condition"].as_object().unwrap().len(), 1);
        assert!(
            material["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["change"]["id"] == watch)
        );
    }
    space.close().await.unwrap();
}
