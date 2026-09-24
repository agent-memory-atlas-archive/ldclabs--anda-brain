use super::*;
mod r6;
mod r8;
use crate::action::*;
use anda_cognitive_nexus::{
    attention::{RuntimePin, RuntimeScope, WakeRecord, WakeState},
    governance::{
        AuthContext,
        store::{GrantDraft, PrincipalDraft},
    },
    nexus::DEFAULT_SPACE,
};
use async_trait::async_trait;
use serde_json::{Value as Json, json};
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
};

const HOST: &str = "kip:principal:r3-host";
const RECIPIENT: &str = "kip:principal:r3-recipient";
use crate::PROFILE;
fn auth(id: &str) -> AuthContext {
    let mut a = AuthContext::principal(id);
    a.auth_method = "trusted-test-authentication".into();
    a
}
fn pin(id: &str) -> RuntimePin {
    RuntimePin {
        id: id.into(),
        digest: anda_cognitive_nexus::content_digest(&json!({"fixture":id})).unwrap(),
    }
}
struct Identity;
#[async_trait]
impl ActionIdentity for Identity {
    async fn authenticate(&self, _: &RuntimeScope) -> Result<AuthContext, BoxError> {
        Ok(auth(HOST))
    }
}

struct Policy {
    context: Mutex<Option<ContextRequest>>,
    proposal: Mutex<Proposal>,
    allowed: AtomicBool,
    calls: AtomicUsize,
    replies: Mutex<Vec<ClarificationResponse>>,
}
#[async_trait]
impl ActionPolicy for Policy {
    async fn context(&self, _: &WakeRecord) -> Result<ContextRequest, BoxError> {
        Ok(self.context.lock().unwrap().clone().unwrap())
    }
    async fn suggest(&self, input: &GateInput) -> Result<Proposal, BoxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!input.packet.action_ready);
        assert!(!input.packet.semantic_complete);
        if let Some(reply) = &input.reply {
            self.replies.lock().unwrap().push(reply.clone());
        }
        Ok(self.proposal.lock().unwrap().clone())
    }
    async fn authorize(&self, request: &ActionRequest) -> Result<(), BoxError> {
        if request.kind == ActionKind::DeliverClarification || self.allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("business permission revoked".into())
        }
    }
    async fn allow_silence(&self, _: &GateInput, _: &str) -> Result<bool, BoxError> {
        Ok(true)
    }
}
#[derive(Default)]
struct Executor {
    sends: Mutex<Vec<ActionRequest>>,
    unknown: AtomicBool,
    idempotent: AtomicBool,
}
#[async_trait]
impl ActionExecutor for Executor {
    fn supports_idempotency(&self) -> bool {
        self.idempotent.load(Ordering::SeqCst)
    }
    async fn authorize(&self, _: &ActionRequest) -> Result<(), BoxError> {
        Ok(())
    }
    async fn dispatch(
        &self,
        r: &ActionRequest,
        p: &DispatchPermit,
    ) -> Result<DeliveryStatus, BoxError> {
        assert!(p.expires_at_ms > unix_ms());
        assert!(p.fence > 0);
        assert!(p.attempt_ref.starts_with("X-"));
        self.sends.lock().unwrap().push(r.clone());
        if self.unknown.load(Ordering::SeqCst) {
            Err("target accepted, response lost".into())
        } else {
            Ok(DeliveryStatus::Finished)
        }
    }
}
fn policy(proposal: Proposal) -> Arc<Policy> {
    Arc::new(Policy {
        context: Mutex::new(None),
        proposal: Mutex::new(proposal),
        allowed: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        replies: Mutex::new(vec![]),
    })
}
fn bindings(p: Arc<Policy>, e: Arc<Executor>) -> ActionBindings {
    ActionBindings {
        policy_pin: pin("r3-policy"),
        binding_pin: pin("r3-adapters"),
        policy: p,
        identity: Arc::new(Identity),
        business: Some(e.clone()),
        clarification: Some(ClarificationBinding {
            executor: e,
            recipient_principal: RECIPIENT.into(),
            reply_timeout_ms: 5000,
        }),
        lookup: None,
        limits: ActionLimits {
            recall: crate::recall_budget::RecallBudget {
                max_tokens: 16000,
                ..Default::default()
            },
            callbacks_ms: 1000,
            lease_ms: 10000,
            retry_ms: 10,
            ..Default::default()
        },
    }
}
async fn principal(space: &Space, id: &str, actions: &[&str]) -> u64 {
    let nexus = space.memory.nexus();
    nexus
        .governance()
        .ensure_principal(PrincipalDraft {
            principal_id: id.into(),
            principal_class: "service".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let row = nexus
        .governance()
        .create_grant(
            GrantDraft {
                space_id: DEFAULT_SPACE.into(),
                grantee_principal: id.into(),
                actions: actions.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            "kip:principal:system",
        )
        .await
        .unwrap();
    row._id
}
async fn setup(
    name: &str,
    b: ActionBindings,
    p: &Policy,
) -> (AppState, Arc<Space>, String, String) {
    setup_store(name, b, p, None).await
}
async fn setup_store(
    name: &str,
    b: ActionBindings,
    p: &Policy,
    store: Option<Arc<dyn object_store::ObjectStore>>,
) -> (AppState, Arc<Space>, String, String) {
    let observer = b.lookup.as_ref().map(|l| l.observer.principal_id.clone());
    let template = test_app_state(name);
    let mut app = if let Some(store) = store {
        template.fork_with_store(store)
    } else {
        template
    };
    app.automatic = true;
    let app = app.with_action_bindings(b).unwrap();
    let space = create_loaded_space(&app, name).await;
    principal(
        &space,
        HOST,
        &[
            "read",
            "read_history",
            "project",
            "create",
            "update",
            "derive",
            "maintain",
            "read_governance_history",
        ],
    )
    .await;
    principal(&space, RECIPIENT, &["read"]).await;
    if let Some(observer) = observer {
        principal(
            &space,
            &observer,
            &["read", "record_outcome", "read_governance_history"],
        )
        .await;
    }
    let target = created_ref(
        &space,
        r#"CREATE CONCEPT ?item {TYPE "Person" NAME "before"}"#,
        Default::default(),
    )
    .await;
    declare_types(&space, &["Coordinate"]).await;
    let anchor=created_ref(&space,r#"MUTATE {CREATE CONCEPT ?preference {TYPE "Coordinate" NAME "known coordinate"} ENSURE PROPOSITION ?item (:target,"prefers",?preference)}"#,kip::param("target",target.clone())).await;
    *p.context.lock().unwrap() = Some(ContextRequest {
        recall_receipt: None,
        anchor,
        required_refs: vec![],
        premises: vec![],
        applied_revisions: vec![],
        task_family: "memory.reminder".into(),
        environment_digest: pin("r3-env").digest,
        tool_versions: BTreeMap::from([("fixture".into(), "v1".into())]),
        deduplication_key: None,
    });
    let watch=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"R3 task",status:"disarmed",condition:{element: :target}}}"#,kip::param("target",target.clone())).await;
    space.attention().arm_watch(watch.clone(), 1).await.unwrap();
    seed_kip(
        &space,
        kip::request_with(
            r#"UPDATE :target SET FIELDS {name:"changed"}"#,
            kip::param("target", target),
        ),
    )
    .await;
    let nexus = space.memory.nexus();
    let session = nexus.session(auth(HOST));
    let fire = session
        .advance_watch(DEFAULT_SPACE, &watch, 2, 1, 200)
        .await
        .unwrap();
    let wake = fire["wake_ref"].as_str().unwrap().to_string();
    (app, space, watch, wake)
}
async fn process(space: &Space, wake: &str) -> ActionStatus {
    let w = space
        .memory
        .nexus()
        .system_session()
        .read_wake(DEFAULT_SPACE, wake)
        .await
        .unwrap();
    space
        .attention()
        .actions()
        .unwrap()
        .process(w)
        .await
        .unwrap()
}
async fn children(space: &Space, parent: &str) -> Vec<WakeRecord> {
    space
        .memory
        .nexus()
        .system_session()
        .list_wakes(DEFAULT_SPACE, None, 200)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|w| w.parent_ref.as_deref() == Some(parent))
        .collect()
}
async fn records(space: &Space, facet: &str) -> Vec<Json> {
    let req = kip::request(format!(
        r#"FIND(?a) WHERE {{?a ACTIVITY {{}} FILTER(IS_NOT_NULL(?a.facets["{facet}"]))}} LIMIT 100"#
    ));
    let response = space.execute_kip_readonly(req).await.unwrap();
    kip::ok_result(&response)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn r3_four_branches_commit_native_outputs_and_continuations_atomically() {
    for (index, proposal, kind, attempts, children_count) in [
        (
            0,
            Proposal::Act {
                rationale: "deliver memory".into(),
                used_refs: vec![],
                payload: json!({"text":"reminder"}),
            },
            "act",
            1,
            1,
        ),
        (
            1,
            Proposal::Ask {
                rationale: "need information".into(),
                used_refs: vec![],
                question: "Which date?".into(),
            },
            "ask",
            0,
            2,
        ),
        (
            2,
            Proposal::Defer {
                reason: "temporary input missing".into(),
            },
            "defer",
            0,
            1,
        ),
        (
            3,
            Proposal::Silence {
                rationale: "host quiet policy".into(),
                used_refs: vec![],
            },
            "silence",
            0,
            0,
        ),
    ] {
        let p = policy(proposal);
        let e = Arc::new(Executor::default());
        let (_app, space, _watch, wake) = setup(
            &format!("r3_branch_{index}"),
            bindings(p.clone(), e.clone()),
            &p,
        )
        .await;
        let status = process(&space, &wake).await;
        assert_eq!(status.state, "committed", "{status:?}");
        assert_eq!(status.decision.as_deref(), Some(kind));
        assert_eq!(records(&space, "AttemptRecord").await.len(), attempts);
        assert_eq!(children(&space, &wake).await.len(), children_count);
        assert!(e.sends.lock().unwrap().is_empty());
        let root = space
            .memory
            .nexus()
            .system_session()
            .read_wake(DEFAULT_SPACE, &wake)
            .await
            .unwrap();
        let WakeState::Completed { receipt_ref } = root.state else {
            panic!("gate did not complete");
        };
        let receipt = space
            .memory
            .nexus()
            .store
            .control_at(DEFAULT_SPACE, &receipt_ref, u64::MAX)
            .await
            .unwrap()
            .unwrap();
        let decisions = records(&space, "DecisionRecord").await;
        assert_eq!(
            decisions[0]["_system"]["space_seq"],
            receipt.value["state"]["commit_seq"]
        );
        // Repeated completion does not produce a second gate/Attempt.
        process(&space, &wake).await;
        assert_eq!(records(&space, "DecisionRecord").await.len(), 1);
        if attempts == 1 {
            let records = records(&space, "AttemptRecord").await;
            assert_eq!(
                records[0]["facets"][format!("{PROFILE}AttemptRecord")]["trial_ref"],
                Json::Null
            );
            assert_eq!(
                records[0]["facets"][format!("{PROFILE}AttemptRecord")]["applied_revisions"],
                json!([])
            );
            let child = children(&space, &wake)
                .await
                .into_iter()
                .find(|w| w.continuation_key.as_deref() == Some("dispatch"))
                .unwrap();
            let sent = process(&space, &child.wake_ref).await;
            assert_eq!(sent.state, "awaiting_outcome", "{sent:?}");
            assert_eq!(status.dispatch_ref, sent.dispatch_ref);
            assert_eq!(e.sends.lock().unwrap().len(), 1);
        }
        space.close().await.unwrap();
    }
}

#[tokio::test]
async fn r3_unknown_premise_and_required_budget_never_dispatch() {
    for tiny in [false, true] {
        let p = policy(Proposal::Act {
            rationale: "unsafe suggestion".into(),
            used_refs: vec![],
            payload: json!({}),
        });
        let e = Arc::new(Executor::default());
        let mut b = bindings(p.clone(), e.clone());
        if tiny {
            b.limits.recall.max_tokens = 1;
        }
        let (_app, space, _, wake) =
            setup(if tiny { "r3_small" } else { "r3_unknown" }, b, &p).await;
        if !tiny {
            let mut c = p.context.lock().unwrap();
            let c = c.as_mut().unwrap();
            c.premises.push(c.anchor.clone());
        }
        let status = process(&space, &wake).await;
        assert_eq!(status.decision.as_deref(), Some("defer"), "{status:?}");
        assert!(records(&space, "AttemptRecord").await.is_empty());
        assert!(e.sends.lock().unwrap().is_empty());
        if tiny {
            assert_eq!(p.calls.load(Ordering::SeqCst), 0);
        }
        space.close().await.unwrap();
    }
}

fn act() -> Proposal {
    Proposal::Act {
        rationale: "deliver the requested memory".into(),
        used_refs: vec![],
        payload: json!({"text":"remember"}),
    }
}
async fn dispatch_child(space: &Space, wake: &str) -> WakeRecord {
    if let Some(child) = children(space, wake)
        .await
        .into_iter()
        .find(|w| w.continuation_key.as_deref() == Some("clarification"))
    {
        let result = process(space, &child.wake_ref).await;
        assert_eq!(result.decision.as_deref(), Some("act"), "{result:?}");
        return children(space, &child.wake_ref)
            .await
            .into_iter()
            .find(|w| w.continuation_key.as_deref() == Some("dispatch"))
            .unwrap();
    }
    children(space, wake)
        .await
        .into_iter()
        .find(|w| w.continuation_key.as_deref() == Some("dispatch"))
        .unwrap()
}

#[tokio::test]
async fn r3_clarification_answer_is_authenticated_and_reenters_gate_without_consent() {
    let p = policy(Proposal::Ask {
        rationale: "clarify date".into(),
        used_refs: vec![],
        question: "Which date?".into(),
    });
    let e = Arc::new(Executor::default());
    let (_app, space, _, wake) = setup("r3_answer", bindings(p.clone(), e.clone()), &p).await;
    assert_eq!(process(&space, &wake).await.state, "committed");
    let child = dispatch_child(&space, &wake).await;
    assert_eq!(
        process(&space, &child.wake_ref).await.state,
        "awaiting_outcome"
    );
    assert_eq!(
        e.sends.lock().unwrap()[0].kind,
        ActionKind::DeliverClarification
    );
    let answer = children(&space, &wake)
        .await
        .into_iter()
        .find(|w| w.continuation_key.as_deref() == Some("answer"))
        .unwrap();
    assert_eq!(
        process(&space, &answer.wake_ref).await.state,
        "awaiting_reply"
    );
    let runtime = space.attention().actions().unwrap();
    let response = ClarificationResponse {
        event_key: "reply-1".into(),
        answer: "Tomorrow".into(),
    };
    assert!(
        runtime
            .respond(wake.clone(), auth(HOST), response.clone())
            .await
            .is_err()
    );
    runtime
        .respond(wake.clone(), auth(RECIPIENT), response.clone())
        .await
        .unwrap();
    runtime
        .respond(wake.clone(), auth(RECIPIENT), response.clone())
        .await
        .unwrap();
    assert!(
        runtime
            .respond(
                wake.clone(),
                auth(RECIPIENT),
                ClarificationResponse {
                    answer: "Changed".into(),
                    ..response.clone()
                }
            )
            .await
            .is_err()
    );
    *p.proposal.lock().unwrap() = act();
    p.allowed.store(false, Ordering::SeqCst);
    sleep(Duration::from_millis(15)).await;
    let result = process(&space, &answer.wake_ref).await;
    assert_eq!(result.decision.as_deref(), Some("defer"), "{result:?}");
    assert_eq!(*p.replies.lock().unwrap(), vec![response]);
    assert_eq!(e.sends.lock().unwrap().len(), 1);
    assert_eq!(records(&space, "AttemptRecord").await.len(), 1);
    let attempts = records(&space, "AttemptRecord").await;
    let decision_ref = attempts[0]["facets"][format!("{PROFILE}AttemptRecord")]["decision_ref"]
        .as_str()
        .unwrap();
    let decisions = records(&space, "DecisionRecord").await;
    let ask = decisions
        .iter()
        .find(|r| r["facets"][format!("{PROFILE}DecisionRecord")]["decision"] == "ask")
        .unwrap();
    let send = decisions.iter().find(|r| r["id"] == decision_ref).unwrap();
    assert_eq!(
        send["facets"][format!("{PROFILE}DecisionRecord")]["decision"],
        "act"
    );
    assert!(
        send["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().or_else(|| r["id"].as_str()) == ask["id"].as_str())
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn r3_clarification_timeout_retains_manual_work_without_reasking_or_consent() {
    let p = policy(Proposal::Ask {
        rationale: "need input".into(),
        used_refs: vec![],
        question: "Proceed?".into(),
    });
    let e = Arc::new(Executor::default());
    let mut b = bindings(p.clone(), e.clone());
    b.clarification.as_mut().unwrap().reply_timeout_ms = 10;
    let (_app, space, _, wake) = setup("r3_timeout", b, &p).await;
    assert_eq!(process(&space, &wake).await.state, "committed");
    let answer = children(&space, &wake)
        .await
        .into_iter()
        .find(|w| w.continuation_key.as_deref() == Some("answer"))
        .unwrap();
    sleep(Duration::from_millis(20)).await;
    let status = process(&space, &answer.wake_ref).await;
    assert_eq!(status.decision.as_deref(), Some("defer"), "{status:?}");
    assert_eq!(p.calls.load(Ordering::SeqCst), 1);
    let manual = children(&space, &answer.wake_ref).await.pop().unwrap();
    sleep(Duration::from_millis(15)).await;
    let status = process(&space, &manual.wake_ref).await;
    assert_eq!(status.state, "blocked", "{status:?}");
    for _ in 0..3 {
        process(&space, &manual.wake_ref).await;
    }
    assert_eq!(p.calls.load(Ordering::SeqCst), 1);
    assert!(e.sends.lock().unwrap().is_empty());
    *p.proposal.lock().unwrap() = Proposal::Silence {
        rationale: "operator resolved the timeout".into(),
        used_refs: vec![],
    };
    let runtime = space.attention().actions().unwrap();
    runtime.retry(manual.wake_ref.clone()).await.unwrap();
    let recovered = process(&space, &manual.wake_ref).await;
    assert_eq!(
        recovered.decision.as_deref(),
        Some("silence"),
        "{recovered:?}"
    );
    assert_eq!(recovered.operator_retries, 1);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r3_revalidates_host_policy_native_principal_context_and_cancellation() {
    for mode in ["policy", "principal", "context", "cancel"] {
        let p = policy(act());
        let e = Arc::new(Executor::default());
        let (_app, space, watch, wake) = setup(
            &format!("r3_revalidate_{mode}"),
            bindings(p.clone(), e.clone()),
            &p,
        )
        .await;
        assert_eq!(process(&space, &wake).await.state, "committed");
        let child = dispatch_child(&space, &wake).await;
        let nexus = space.memory.nexus();
        let s = nexus.session(auth(HOST));
        match mode {
            "policy" => p.allowed.store(false, Ordering::SeqCst),
            "principal" => {
                nexus
                    .system_session()
                    .set_principal_status(DEFAULT_SPACE, HOST, "suspended")
                    .await
                    .unwrap();
            }
            "context" => {
                let v = element_version(&space, &watch).await;
                seed_kip(&space,kip::request_with(format!(r#"UPDATE :watch SET FIELDS {{name:"changed after gate"}} EXPECT VERSION {v}"#),kip::param("watch",watch))).await;
            }
            "cancel" => {
                s.claim_wake(
                    DEFAULT_SPACE,
                    &child.wake_ref,
                    1,
                    0,
                    &kip::timestamp(unix_ms() + 10000),
                )
                .await
                .unwrap();
                s.cancel_wake(DEFAULT_SPACE, &child.wake_ref, 2, 1, "cancelled by host")
                    .await
                    .unwrap();
                let parent = space
                    .attention()
                    .actions()
                    .unwrap()
                    .status(&wake)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    s.begin_wake_dispatch(
                        DEFAULT_SPACE,
                        &child.wake_ref,
                        2,
                        1,
                        parent.attempt_ref.as_deref().unwrap(),
                        false,
                        false
                    )
                    .await
                    .is_err()
                );
            }
            _ => unreachable!(),
        }
        let status = process(&space, &child.wake_ref).await;
        assert_ne!(status.state, "awaiting_outcome", "{status:?}");
        assert!(e.sends.lock().unwrap().is_empty());
        space.close().await.unwrap();
    }
}

#[tokio::test]
async fn r3_lost_remote_ack_survives_cold_reload_with_one_nonidempotent_attempt() {
    let p = policy(act());
    let e = Arc::new(Executor::default());
    e.unknown.store(true, Ordering::SeqCst);
    let b = bindings(p.clone(), e.clone());
    let (app, space, _, wake) = setup("r3_restart", b.clone(), &p).await;
    assert_eq!(process(&space, &wake).await.state, "committed");
    let child = dispatch_child(&space, &wake).await;
    let status = process(&space, &child.wake_ref).await;
    assert_eq!(status.state, "outcome_unknown", "{status:?}");
    let attempt = e.sends.lock().unwrap()[0].attempt_id.clone();
    space.close().await.unwrap();
    let mut restarted = app.fork_with_store(app.object_store());
    restarted.automatic = true;
    let restarted = restarted.with_action_bindings(b).unwrap();
    let loaded = restarted
        .load_space_with("r3_restart", false, false)
        .await
        .unwrap();
    sleep(Duration::from_millis(15)).await;
    let status = process(&loaded, &child.wake_ref).await;
    assert_eq!(status.state, "blocked", "{status:?}");
    assert_eq!(e.sends.lock().unwrap().len(), 1);
    assert_eq!(e.sends.lock().unwrap()[0].attempt_id, attempt);
    assert_eq!(records(&loaded, "AttemptRecord").await.len(), 1);
    loaded.close().await.unwrap();
}

#[tokio::test]
async fn r3_duplicate_logical_operation_silences_only_after_original_gate_committed() {
    let p = policy(act());
    let e = Arc::new(Executor::default());
    let (_app, space, watch, wake) =
        setup("r3_duplicate", bindings(p.clone(), e.clone()), &p).await;
    p.context
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .deduplication_key = Some("reminder:one".into());
    assert_eq!(
        process(&space, &wake).await.decision.as_deref(),
        Some("act")
    );
    let nexus = space.memory.nexus();
    let s = nexus.session(auth(HOST));
    // Two independently fired Watches propose the same logical operation.
    let read = space
        .execute_kip_readonly(kip::request_with(
            r#"FIND(?w.attributes.condition.element) WHERE {?w CONCEPT {id: :id}}"#,
            kip::param("id", watch.clone()),
        ))
        .await
        .unwrap();
    let target = kip::ok_result(&read).unwrap()[0].as_str().unwrap();
    let second=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"same logical task",status:"disarmed",condition:{element: :target}}}"#,kip::param("target",target)).await;
    space
        .attention()
        .arm_watch(second.clone(), 1)
        .await
        .unwrap();
    seed_kip(
        &space,
        kip::request_with(
            r#"UPDATE :id SET FIELDS {name:"new trigger"}"#,
            kip::param("id", target),
        ),
    )
    .await;
    let fire = s
        .advance_watch(DEFAULT_SPACE, &second, 2, 1, 200)
        .await
        .unwrap();
    let next = fire["wake_ref"].as_str().unwrap();
    let result = process(&space, next).await;
    assert_eq!(result.decision.as_deref(), Some("silence"), "{result:?}");
    assert_eq!(records(&space, "AttemptRecord").await.len(), 1);
    assert!(children(&space, next).await.is_empty());
    space.close().await.unwrap();
}

struct Lookup {
    states:
        Mutex<std::collections::VecDeque<anda_cognitive_nexus::attention::DispatchLookupStatus>>,
    calls: Mutex<Vec<String>>,
}
#[async_trait]
impl ActionLookup for Lookup {
    async fn lookup(
        &self,
        r: &ActionRequest,
        _: &str,
    ) -> Result<(AuthContext, anda_cognitive_nexus::attention::DispatchLookup), BoxError> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(r.attempt_id.clone());
        Ok((
            auth("kip:principal:r3-lookup"),
            anda_cognitive_nexus::attention::DispatchLookup {
                observation_key: format!("lookup-{}", calls.len()),
                observed_at: anda_cognitive_nexus::time::now(),
                configuration_digest: pin("r3-lookup-config").digest,
                status: self
                    .states
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(anda_cognitive_nexus::attention::DispatchLookupStatus::Unknown),
            },
        ))
    }
}

#[tokio::test]
async fn r3_authoritative_not_started_reuses_attempt_and_unknown_cannot_resend() {
    use anda_cognitive_nexus::attention::{DispatchLookupObserver, DispatchLookupStatus::*};
    let p = policy(act());
    let e = Arc::new(Executor::default());
    e.unknown.store(true, Ordering::SeqCst);
    let lookup = Arc::new(Lookup {
        states: Mutex::new([NotStarted, Unknown, Finished].into()),
        calls: Mutex::new(vec![]),
    });
    let mut b = bindings(p.clone(), e.clone());
    b.limits.max_retries = 8;
    b.lookup = Some(LookupBinding {
        observer: DispatchLookupObserver {
            binding: b.binding_pin.clone(),
            principal_id: "kip:principal:r3-lookup".into(),
            configuration_digest: pin("r3-lookup-config").digest,
        },
        client: lookup.clone(),
    });
    let (_app, space, _, wake) = setup("r3_lookup", b, &p).await;
    assert_eq!(process(&space, &wake).await.state, "committed");
    let child = dispatch_child(&space, &wake).await;
    assert_eq!(
        process(&space, &child.wake_ref).await.state,
        "outcome_unknown"
    );
    for expected in [
        "ready",
        "outcome_unknown",
        "outcome_unknown",
        "awaiting_outcome",
        "blocked",
    ] {
        sleep(Duration::from_millis(15)).await;
        let result = process(&space, &child.wake_ref).await;
        assert_eq!(result.state, expected, "{result:?}");
    }
    assert_eq!(e.sends.lock().unwrap().len(), 2);
    let id = e.sends.lock().unwrap()[0].attempt_id.clone();
    assert!(e.sends.lock().unwrap().iter().all(|r| r.attempt_id == id));
    assert_eq!(
        *lookup.calls.lock().unwrap(),
        vec![id.clone(), id.clone(), id]
    );
    assert_eq!(records(&space, "AttemptRecord").await.len(), 1);
    let outcomes = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?e) WHERE {?e EVIDENCE {} FILTER(IS_NOT_NULL(?e.facets["OutcomeRecord"]))}"#,
        ))
        .await
        .unwrap();
    assert_eq!(kip::ok_result(&outcomes).unwrap(), &json!([]));
    space.close().await.unwrap();
}

#[tokio::test]
async fn r3_gate_directory_ack_failure_recovers_native_receipt_through_child() {
    use anda_object_store::fault::{FaultOp, FaultRule, FaultStore};
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let p = policy(act());
    let e = Arc::new(Executor::default());
    let b = bindings(p.clone(), e.clone());
    let (app, space, _, wake) =
        setup_store("r3_gate_ack", b.clone(), &p, Some(Arc::new(store))).await;
    faults.push_rule(FaultRule {
        skip: 1,
        ..FaultRule::fail_once(FaultOp::Put, "/actions/jobs/")
    });
    let w = space
        .memory
        .nexus()
        .system_session()
        .read_wake(DEFAULT_SPACE, &wake)
        .await
        .unwrap();
    assert!(
        space
            .attention()
            .actions()
            .unwrap()
            .process(w)
            .await
            .is_err()
    );
    assert!(matches!(
        space
            .memory
            .nexus()
            .system_session()
            .read_wake(DEFAULT_SPACE, &wake)
            .await
            .unwrap()
            .state,
        WakeState::Completed { .. }
    ));
    let child = dispatch_child(&space, &wake).await;
    faults.reset();
    space.close().await.unwrap();
    let mut restarted = app.fork_with_store(app.object_store());
    restarted.automatic = true;
    let restarted = restarted.with_action_bindings(b).unwrap();
    let loaded = restarted
        .load_space_with("r3_gate_ack", false, false)
        .await
        .unwrap();
    let sent = process(&loaded, &child.wake_ref).await;
    assert_eq!(sent.state, "awaiting_outcome", "{sent:?}");
    assert_eq!(
        loaded
            .attention()
            .actions()
            .unwrap()
            .status(&wake)
            .await
            .unwrap()
            .unwrap()
            .state,
        "committed"
    );
    assert_eq!(records(&loaded, "DecisionRecord").await.len(), 1);
    assert_eq!(records(&loaded, "AttemptRecord").await.len(), 1);
    assert_eq!(e.sends.lock().unwrap().len(), 1);
    loaded.close().await.unwrap();
}

#[tokio::test]
async fn r3_owned_native_commit_drains_when_waiter_is_cancelled() {
    use anda_object_store::fault::{FaultGate, FaultKind, FaultOp, FaultRule, FaultStore};
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let p = policy(Proposal::Silence {
        rationale: "configured quiet time".into(),
        used_refs: vec![],
    });
    let e = Arc::new(Executor::default());
    let b = bindings(p.clone(), e);
    let (app, space, _, wake) = setup_store("r3_drain", b.clone(), &p, Some(Arc::new(store))).await;
    let nexus = space.memory.nexus();
    let s = nexus.session(auth(HOST));
    s.claim_wake(
        DEFAULT_SPACE,
        &wake,
        1,
        0,
        &kip::timestamp(unix_ms() + 10000),
    )
    .await
    .unwrap();
    let gate = FaultGate::new();
    faults.push_rule(FaultRule {
        kind: FaultKind::PauseAfter(gate.clone()),
        ..FaultRule::fail_once(FaultOp::Put, "kip_control_records")
    });
    let waiter = {
        let space = space.clone();
        let wake = wake.clone();
        tokio::spawn(async move { process(&space, &wake).await })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_entered())
        .await
        .unwrap();
    waiter.abort();
    let _ = waiter.await;
    assert!(space.is_busy());
    let close = {
        let space = space.clone();
        tokio::spawn(async move { space.close().await })
    };
    sleep(Duration::from_millis(20)).await;
    assert!(!close.is_finished());
    gate.release();
    close.await.unwrap().unwrap();
    faults.reset();
    let mut restarted = app.fork_with_store(app.object_store());
    restarted.automatic = true;
    let restarted = restarted.with_action_bindings(b).unwrap();
    let loaded = restarted
        .load_space_with("r3_drain", false, false)
        .await
        .unwrap();
    assert_eq!(
        loaded
            .attention()
            .actions()
            .unwrap()
            .status(&wake)
            .await
            .unwrap()
            .unwrap()
            .state,
        "committed"
    );
    assert_eq!(records(&loaded, "DecisionRecord").await.len(), 1);
    loaded.close().await.unwrap();
}

#[tokio::test]
async fn r3_scheduler_pages_actions_and_respects_disabled_and_isolated_hosts() {
    let p = policy(Proposal::Silence {
        rationale: "host quiet rule".into(),
        used_refs: vec![],
    });
    let e = Arc::new(Executor::default());
    let mut b = bindings(p.clone(), e.clone());
    b.limits.per_pass = 1;
    let (app, space, watch, wake) = setup("r3_scheduled", b, &p).await;
    let read = space
        .execute_kip_readonly(kip::request_with(
            r#"FIND(?w.attributes.condition.element) WHERE {?w CONCEPT {id: :id}}"#,
            kip::param("id", watch),
        ))
        .await
        .unwrap();
    let target = kip::ok_result(&read).unwrap()[0].as_str().unwrap();
    for i in 0..4 {
        let w=created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"fair gate",status:"disarmed",condition:{element: :target}}}"#,kip::param("target",target)).await;
        space.attention().arm_watch(w.clone(), 1).await.unwrap();
        let name = format!("tick-{i}");
        seed_kip(
            &space,
            kip::request_with(
                r#"UPDATE :id SET FIELDS {name: :name}"#,
                serde_json::Map::from_iter([
                    ("id".into(), json!(target)),
                    ("name".into(), json!(name)),
                ]),
            ),
        )
        .await;
        space
            .memory
            .nexus()
            .session(auth(HOST))
            .advance_watch(DEFAULT_SPACE, &w, 2, 1, 200)
            .await
            .unwrap();
    }
    space.attention().set_enabled(false).await.unwrap();
    space.attention().tick().await.unwrap();
    assert_eq!(p.calls.load(Ordering::SeqCst), 0);
    space.attention().set_enabled(true).await.unwrap();
    let actions = space.attention().actions().unwrap();
    actions.set_enabled(false).await.unwrap();
    space.attention().tick().await.unwrap();
    assert_eq!(p.calls.load(Ordering::SeqCst), 0);
    assert!(!actions.enabled().await.unwrap());
    actions.set_enabled(true).await.unwrap();
    for _ in 0..16 {
        let pass = space.attention().tick().await.unwrap();
        assert!(pass.actions_processed <= 1);
    }
    assert_eq!(records(&space, "DecisionRecord").await.len(), 5);
    assert_eq!(
        space
            .attention()
            .actions()
            .unwrap()
            .status(&wake)
            .await
            .unwrap()
            .unwrap()
            .state,
        "committed"
    );
    let fork = app.fork_with_store(Arc::new(InMemory::new()));
    assert!(fork.action_bindings.is_none());
    assert!(!fork.automatic);
    assert_eq!(fork.attention_tick().await.unwrap().loaded, 0);
    let state = crate::cognitive::MemoryRuntimeTool::new(space.memory.clone(), space.attention())
        .execute(
            crate::cognitive::RuntimeArgs {
                operation: "status".into(),
                target_ref: None,
                expected_version: None,
                content: None,
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(state["actions"]["enabled"], true);
    assert!(state["actions"]["ready"].is_null());
    assert!(e.sends.lock().unwrap().is_empty());
    space.close().await.unwrap();
}

#[tokio::test]
async fn r3_native_admission_rejects_a_revision_replaced_after_gate() {
    let p = policy(act());
    let e = Arc::new(Executor::default());
    let (_app, space, watch, _old_wake) =
        setup("r3_revision", bindings(p.clone(), e.clone()), &p).await;
    let behavior = json!({"task_family":"memory.reminder","procedure":"deliver approved reminder"});
    let response=space.run_kip_settlement(kip::request_with(r#"MUTATE {
        CREATE CONCEPT ?skill {TYPE "Skill" SET ATTRIBUTES {skill_class:"workflow",summary:"host-selected procedure",status:"proposed"} SET STRUCTURAL {("current_revision",?first)}}
        CREATE CONCEPT ?first {TYPE "SkillRevision" SET ATTRIBUTES {task_family:"memory.reminder",procedure:"deliver approved reminder",behavior_digest: :digest} SET STRUCTURAL {("revision_of",?skill)}}
        CREATE CONCEPT ?second {TYPE "SkillRevision" SET ATTRIBUTES {task_family:"memory.reminder",procedure:"deliver approved reminder",behavior_digest: :digest} SET STRUCTURAL {("revision_of",?skill)}}
    }"#,kip::param("digest",anda_cognitive_nexus::content_digest(&behavior).unwrap()))).await.unwrap();
    assert!(
        kip::succeeded(&response),
        "{}",
        kip::error_message(&response)
    );
    let handles = &kip::ok_result(&response).unwrap()["handles"];
    let first = handles["first"].as_str().unwrap().to_string();
    let second = handles["second"].as_str().unwrap().to_string();
    let skill = handles["skill"].as_str().unwrap().to_string();
    let nexus = space.memory.nexus();
    let host = nexus.session(auth(HOST));
    for id in [&first, &second] {
        nexus
            .system_session()
            .elevate_authority(DEFAULT_SPACE, id.parse().unwrap(), "executable")
            .await
            .unwrap();
    }
    let anchor = p.context.lock().unwrap().as_ref().unwrap().anchor.clone();
    let projection = kip::execute_readonly_request(
        &host,
        &kip::request_with(
            "FIND(?b) WHERE {?p PROPOSITION(id: :id) ?b BELIEF(?p)}",
            kip::param("id", anchor.clone()),
        ),
    )
    .await;
    let basis = &kip::ok_result(&projection).unwrap()[0]["basis"];
    seed_kip(&space,kip::request_with(r#"CREATE ACTIVITY ?validation {SET FIELDS {activity_class:"dependency_validation",status:"completed"}
      SET FACET "DependencyBasis" {basis_seq: :seq,policy_basis: :basis,groups:[{role:"context",pins:[{id: :anchor,version:1}]}]}
      SET STRUCTURAL {("inputs",:anchor) ("outputs",:first) ("outputs",:second)}}"#,json!({"seq":basis["snapshot_seq"],"basis":basis,"anchor":anchor,"first":first,"second":second}).as_object().unwrap().clone())).await;
    p.context
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .applied_revisions = vec![first.clone()];
    *p.proposal.lock().unwrap() = Proposal::Act {
        rationale: "host explicitly selected this revision".into(),
        used_refs: vec![first],
        payload: json!({"text":"reminder"}),
    };
    // Provisioning executable authority changes the native authorization basis.
    // Arm a fresh generation after provisioning, never reuse the old wake.
    let version = element_version(&space, &watch).await;
    space
        .attention()
        .arm_watch(watch.clone(), version)
        .await
        .unwrap();
    let read = space
        .execute_kip_readonly(kip::request_with(
            r#"FIND(?w.attributes.condition.element) WHERE {?w CONCEPT {id: :id}}"#,
            kip::param("id", watch.clone()),
        ))
        .await
        .unwrap();
    let target = kip::ok_result(&read).unwrap()[0].as_str().unwrap();
    seed_kip(
        &space,
        kip::request_with(
            r#"UPDATE :target SET FIELDS {name:"after authority provisioning"}"#,
            kip::param("target", target),
        ),
    )
    .await;
    let fired = host
        .advance_watch(DEFAULT_SPACE, &watch, version + 1, 2, 200)
        .await
        .unwrap();
    let wake = fired["wake_ref"].as_str().unwrap().to_string();
    let gate = process(&space, &wake).await;
    assert_eq!(gate.decision.as_deref(), Some("act"), "{gate:?}");
    let child = dispatch_child(&space, &wake).await;
    let version = element_version(&space, &skill).await;
    seed_kip(&space,kip::request_with(format!(r#"UPDATE :skill SET STRUCTURAL {{("current_revision",:revision)}} EXPECT VERSION {version}"#),json!({"skill":skill,"revision":second}).as_object().unwrap().clone())).await;
    let result = process(&space, &child.wake_ref).await;
    assert_eq!(result.state, "retrying", "{result:?}");
    assert!(result.reason.unwrap().contains("no longer current"));
    assert!(e.sends.lock().unwrap().is_empty());
    space.close().await.unwrap();
}

#[tokio::test]
async fn r3_verified_idempotency_has_one_attempt_and_a_persistent_retry_bound() {
    let p = policy(act());
    let e = Arc::new(Executor::default());
    e.unknown.store(true, Ordering::SeqCst);
    e.idempotent.store(true, Ordering::SeqCst);
    let mut b = bindings(p.clone(), e.clone());
    b.limits.max_retries = 1;
    let (_app, space, _, wake) = setup("r3_idempotent", b, &p).await;
    assert_eq!(process(&space, &wake).await.state, "committed");
    let child = dispatch_child(&space, &wake).await;
    for _ in 0..4 {
        process(&space, &child.wake_ref).await;
        sleep(Duration::from_millis(15)).await;
    }
    let status = space
        .attention()
        .actions()
        .unwrap()
        .status(&child.wake_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.state, "blocked", "{status:?}");
    let sends = e.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].attempt_id, sends[1].attempt_id);
    assert_eq!(records(&space, "AttemptRecord").await.len(), 1);
    space.close().await.unwrap();
}

#[tokio::test]
async fn r3_missing_binding_and_undelivered_used_refs_defer_without_attempts() {
    for missing in [true, false] {
        let proposal = if missing {
            act()
        } else {
            Proposal::Act {
                rationale: "model invented a use".into(),
                used_refs: vec!["X-9999".into()],
                payload: json!({}),
            }
        };
        let p = policy(proposal);
        let e = Arc::new(Executor::default());
        let mut b = bindings(p.clone(), e.clone());
        if missing {
            b.business = None;
        }
        let (_app, space, _, wake) = setup(
            if missing {
                "r3_no_binding"
            } else {
                "r3_fake_use"
            },
            b,
            &p,
        )
        .await;
        let result = process(&space, &wake).await;
        assert_eq!(result.decision.as_deref(), Some("defer"), "{result:?}");
        assert!(records(&space, "AttemptRecord").await.is_empty());
        assert!(e.sends.lock().unwrap().is_empty());
        space.close().await.unwrap();
    }
}
