use super::*;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};

struct Provider {
    finish: &'static str,
    model: &'static str,
    calls: AtomicUsize,
}
async fn completion(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Json<Value> {
    assert_eq!(headers["authorization"], "Bearer local-r7-provider-token");
    assert_eq!(request["stream"], false);
    assert_eq!(request["max_tokens"], 4096);
    assert!(request.get("tools").is_none());
    provider.calls.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"model":provider.model,"choices":[{"finish_reason":provider.finish,"message":{"role":"assistant","content":reply(&request,Mode::Match)}}]}),
    )
}
#[tokio::test]
async fn r7_compiled_http_adapter_pins_model_and_rejects_provider_truncation() {
    for (i, (finish, model)) in [
        ("stop", "fixture-pinned-v1"),
        ("length", "fixture-pinned-v1"),
        ("stop", "substituted-model"),
        ("stop", "fixture-pinned-v1"),
    ]
    .into_iter()
    .enumerate()
    {
        let provider = Arc::new(Provider {
            finish,
            model,
            calls: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let server = {
            let cancel = cancel.clone();
            let router = Router::new()
                .route("/v1/chat/completions", post(completion))
                .with_state(provider.clone());
            tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(cancel.cancelled_owned())
                    .await
                    .unwrap()
            })
        };
        let mut contract = contract();
        contract.endpoint = format!("http://{address}/v1/chat/completions");
        let name = format!("r7_http_{i}");
        let mut base = runtime_app(Arc::new(InMemory::new()), fast_policy());
        base.automatic = true;
        let config:crate::runtime_api::config::RuntimeConfig=serde_json::from_value(json!({"format":crate::runtime_api::FORMAT,"spaces":{&name:{"bootstrap":true,"subjects":[],"audience":[],"adapter":null,
            "semantic":{"contract":contract,"api_key_env":"R7_MODEL_TOKEN"}}}})).unwrap();
        let app = if i == 3 {
            let mut bindings = config
                .resolve(|_| Some("local-r7-provider-token".into()))
                .unwrap();
            // Re-labeling the already resolved client's endpoint must not send
            // the page to its old destination under the new configuration pin.
            bindings
                .spaces
                .get_mut(&name)
                .unwrap()
                .semantic
                .as_mut()
                .unwrap()
                .config
                .endpoint = "http://127.0.0.1:2/v1/chat/completions".into();
            base.with_memory_runtime_bindings(bindings).unwrap()
        } else {
            base.with_runtime_config(config, |name| {
                (name == "R7_MODEL_TOKEN").then(|| "local-r7-provider-token".into())
            })
            .unwrap()
        };
        let space = create_loaded_space(&app, &name).await;
        let target = target(&space).await;
        let watch = new_watch(&space, &target, "delta", None, true).await;
        change(&space, &target, "actual HTTP model fixture").await;
        let before = watch_state(&space, &watch).await;
        let pass = space
            .attention()
            .semantic()
            .unwrap()
            .run_once()
            .await
            .unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), usize::from(i != 3));
        if i == 0 {
            assert_eq!(pass.fired, 1, "{pass:?}");
        } else {
            assert_eq!(pass.advanced, 0);
            assert_eq!(watch_state(&space, &watch).await, before);
        }
        space.close().await.unwrap();
        cancel.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn r7_automatic_model_work_does_not_delay_structured_scan_and_shutdown_drains_it() {
    let gate = EvalGate::new();
    let evaluator = Arc::new(Evaluator {
        mode: Mutex::new(Mode::Match),
        calls: AtomicUsize::new(0),
        requests: Default::default(),
        gate: Some(gate.clone()),
    });
    let mut cfg = contract();
    cfg.automatic = true;
    let (app, space, _, watch) = fixture("r7_background", evaluator.clone(), cfg).await;
    let other = target(&space).await;
    let structured = new_watch(&space, &other, "delta", None, false).await;
    app.attention_tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), gate.wait_entered())
        .await
        .unwrap();
    change(&space, &other, "ordinary structured trigger").await;
    // Explicit discovery mark makes the second pass due; it does not await LLM I/O.
    space.attention().register_work().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), app.attention_tick())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(watch_state(&space, &structured).await[0], "fired");
    assert_eq!(watch_state(&space, &watch).await[0], "armed");
    assert!(space.is_busy());
    let closing = {
        let space = space.clone();
        tokio::spawn(async move { space.close().await })
    };
    sleep(Duration::from_millis(20)).await;
    assert!(!closing.is_finished());
    gate.release();
    closing.await.unwrap().unwrap();
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn r7_dropped_manual_waiter_still_finishes_the_owned_page_commit() {
    let gate = EvalGate::new();
    let evaluator = Arc::new(Evaluator {
        mode: Mutex::new(Mode::Match),
        calls: AtomicUsize::new(0),
        requests: Default::default(),
        gate: Some(gate.clone()),
    });
    let (_app, space, _, watch) = fixture("r7_owned", evaluator, contract()).await;
    let task = {
        let runtime = space.attention().semantic().unwrap();
        tokio::spawn(async move { runtime.run_once().await })
    };
    tokio::time::timeout(Duration::from_secs(3), gate.wait_entered())
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    assert!(space.is_busy());
    gate.release();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if watch_state(&space, &watch).await[0] == "fired" {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(wake_count(&space).await, 1);
    space.close().await.unwrap();
}

#[test]
fn r7_template_is_opt_in_and_configuration_pins_all_semantic_inputs() {
    use crate::runtime_api::config::RuntimeConfig;
    let cfg: RuntimeConfig =
        serde_json::from_str(include_str!("../../../../../semantic.runtime.example.json")).unwrap();
    let bindings = cfg.resolve(|_| Some("test-only-secret".into())).unwrap();
    let semantic = bindings
        .spaces
        .values()
        .next()
        .unwrap()
        .semantic
        .as_ref()
        .unwrap();
    assert!(!semantic.config.automatic);
    let initial = semantic.config.pin().unwrap();
    let mut changed = semantic.config.clone();
    changed.model = "different-model-revision".into();
    assert_ne!(initial, changed.pin().unwrap());
    let mut changed = semantic.config.clone();
    changed.limits.input_tokens += 1;
    assert_ne!(initial, changed.pin().unwrap());
    let mut changed = semantic.config.clone();
    changed.endpoint = "http://remote.example/v1/chat/completions".into();
    assert!(changed.validate().is_err());
    let cfg: RuntimeConfig =
        serde_json::from_str(include_str!("../../../../../semantic.runtime.example.json")).unwrap();
    assert!(cfg.resolve(|_| None).is_err());
}

#[tokio::test]
async fn r7_predeadline_page_does_not_gain_deadline_coverage_when_model_finishes_late() {
    let gate = EvalGate::new();
    let evaluator = Arc::new(Evaluator {
        mode: Mutex::new(Mode::NoMatch),
        calls: AtomicUsize::new(0),
        requests: Default::default(),
        gate: Some(gate.clone()),
    });
    let app = semantic_app(
        "r7_deadline",
        Arc::new(InMemory::new()),
        contract(),
        evaluator,
    );
    let space = create_loaded_space(&app, "r7_deadline").await;
    let target = target(&space).await;
    let due = unix_ms() + 600;
    let watch = new_watch(&space, &target, "silence", Some(due), true).await;
    change(&space, &target, "not a reply").await;
    let runtime = space.attention().semantic().unwrap();
    let task = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.run_once().await })
    };
    tokio::time::timeout(Duration::from_secs(3), gate.wait_entered())
        .await
        .unwrap();
    let progress = runtime.progress(&watch).await.unwrap().unwrap();
    let page = session(&space)
        .read_prepared_watch_page(DEFAULT_SPACE, progress.ticket_ref.as_deref().unwrap())
        .await
        .unwrap();
    assert!(!page.deadline_covered);
    sleep(Duration::from_millis(due.saturating_sub(unix_ms()) + 20)).await;
    gate.release();
    let pass = task.await.unwrap().unwrap();
    assert_eq!(pass.fired, 0);
    assert_eq!(watch_state(&space, &watch).await[0], "armed");
    let pass = runtime.run_once().await.unwrap();
    assert_eq!(pass.fired, 1, "{pass:?}");
    assert_eq!(pass.calls, 0);
    space.close().await.unwrap();
}
