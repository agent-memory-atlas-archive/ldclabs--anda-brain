use super::*;
use anda_cognitive_nexus::{
    CognitiveNexus,
    attention::{WakeResume, WakeResumeInput, WakeResumeVerifier, WakeRetry, WakeState},
    content_digest,
    nexus::{DEFAULT_SPACE, Session},
};
use futures::FutureExt;
use journal::{Dedup, Job, Reply, key};
use object_store::PutMode;
use serde_json::json;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct ActionRuntime {
    pub(super) receipts: parking_lot::RwLock<Option<Arc<crate::recall_receipt::RecallReceipts>>>,
    pub(super) nexus: Arc<CognitiveNexus>,
    pub(super) directory: Arc<crate::attention::Directory>,
    pub(super) bindings: Arc<ActionBindings>,
    automatic: bool,
    evaluator: Option<RuntimePin>,
    gate: Mutex<()>,
    tasks: crate::runtime::DurableTasks,
    resume_bindings: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

struct Resume {
    directory: Arc<crate::attention::Directory>,
    scope: RuntimeScope,
    reference: String,
    nexus: std::sync::Weak<CognitiveNexus>,
    timeout: Duration,
}
#[async_trait]
impl WakeResumeVerifier for Resume {
    async fn verify(&self, _input: WakeResumeInput) -> Result<bool, anda_kip::KipError> {
        let check = async {
            let job = self
                .directory
                .read::<Job>(&key(&self.scope, "jobs", &self.reference)?)
                .await?
                .ok_or("action journal missing")?
                .value;
            job.validate(&self.scope, &self.reference)?;
            if job.status.state == "pending" {
                return Ok(true);
            }
            if let Some(reference) = job.status.dispatch_ref {
                let row = self
                    .nexus
                    .upgrade()
                    .ok_or("action runtime closed")?
                    .system_session()
                    .read_control(DEFAULT_SPACE, &reference, None)
                    .await?;
                return Ok(row.is_some_and(|r| {
                    r.value["state"] == "completed" || r.value["state"] == "ready"
                }));
            }
            Ok(false)
        };
        let result: Result<bool, BoxError> = match tokio::time::timeout(self.timeout, check).await {
            Ok(result) => result,
            Err(e) => Err(e.into()),
        };
        result.map_err(|e| anda_kip::KipError::constraint_violation(e.to_string()))
    }
}

impl ActionRuntime {
    /// Trusted Rust governance access for provisioning actual host/observer
    /// Principals. This capability is never exposed through model tools.
    pub fn nexus(&self) -> Arc<CognitiveNexus> {
        self.nexus.clone()
    }
    pub async fn enabled(&self) -> Result<bool, BoxError> {
        if !self.automatic {
            return Ok(false);
        }
        let scope = self.scope().await?;
        let setting = self
            .directory
            .read::<journal::Switch>(&key(&scope, "settings", "enabled")?)
            .await?;
        match setting {
            Some(s) if s.value.scope != scope => Err("action switch scope mismatch".into()),
            Some(s) => Ok(s.value.enabled),
            None => Ok(true),
        }
    }
    /// Pauses new gate/dispatch work while Watch scheduling and retained queues
    /// continue. Already admitted writes/callbacks drain before this returns.
    pub async fn set_enabled(self: &Arc<Self>, enabled: bool) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                if enabled && !this.automatic {
                    return Err("isolated host cannot enable actions".into());
                }
                let scope = this.scope().await?;
                let key = key(&scope, "settings", "enabled")?;
                let old = this.directory.read::<journal::Switch>(&key).await?;
                this.directory
                    .put(
                        &key,
                        &journal::Switch { scope, enabled },
                        old.map_or(PutMode::Create, |v| PutMode::Update(v.version)),
                    )
                    .await
            })
            .await
    }
    pub(crate) fn new(
        nexus: Arc<CognitiveNexus>,
        directory: Arc<crate::attention::Directory>,
        bindings: Arc<ActionBindings>,
        automatic: bool,
        evaluator: Option<RuntimePin>,
    ) -> Arc<Self> {
        Arc::new(Self {
            evaluator,
            receipts: Default::default(),
            nexus,
            directory,
            bindings,
            automatic,
            gate: Mutex::new(()),
            tasks: Default::default(),
            resume_bindings: Default::default(),
        })
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.tasks.is_busy()
    }
    pub(crate) fn bind_receipts(&self, receipts: Arc<crate::recall_receipt::RecallReceipts>) {
        *self.receipts.write() = Some(receipts);
    }
    pub(crate) async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }
    pub(crate) fn per_pass(&self) -> usize {
        self.bindings.limits.per_pass
    }
    pub(crate) fn pins(&self) -> RuntimePins {
        let mut pins = self.bindings.pins();
        pins.evaluator = self.evaluator.clone();
        pins
    }
    pub(crate) async fn session(&self, scope: &RuntimeScope) -> Result<Session, BoxError> {
        let auth = self
            .bounded(self.bindings.identity.authenticate(scope))
            .await?;
        if !principal_valid(&auth.principal_id)
            || auth.auth_method.is_empty()
            || auth.auth_strength == "none"
            || !auth.delegation_chain.is_empty()
        {
            return Err("actions require a directly authenticated host principal".into());
        }
        Ok(self.nexus.session(auth))
    }
    pub(super) async fn bounded<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, BoxError>>,
    ) -> Result<T, BoxError> {
        tokio::time::timeout(
            Duration::from_millis(self.bindings.limits.callbacks_ms),
            std::panic::AssertUnwindSafe(future).catch_unwind(),
        )
        .await?
        .map_err(|_| "action callback panicked")?
    }
    pub(crate) async fn install(&self) -> Result<(), BoxError> {
        if let Some(lookup) = &self.bindings.lookup {
            let s = self.nexus.system_session();
            let key = format!(
                "attention-lookup-observer/v1/{}",
                &content_digest(&json!(lookup.observer.binding))?[7..]
            );
            match s.read_control(DEFAULT_SPACE, &key, None).await? {
                Some(old) if old.value != json!(lookup.observer) => {
                    return Err(
                        "installed lookup observer differs; explicit host migration required"
                            .into(),
                    );
                }
                Some(_) => {}
                None => {
                    s.set_dispatch_lookup_observer(DEFAULT_SPACE, 0, lookup.observer.clone())
                        .await?;
                }
            }
        }
        Ok(())
    }
    pub(super) async fn load(
        &self,
        scope: &RuntimeScope,
        reference: &str,
    ) -> Result<Option<Job>, BoxError> {
        let row = self
            .directory
            .read::<Job>(&key(scope, "jobs", reference)?)
            .await?;
        if let Some(row) = &row {
            row.value.validate(scope, reference)?;
        }
        Ok(row.map(|v| v.value))
    }
    pub(super) async fn answered(
        &self,
        scope: &RuntimeScope,
        gate: &str,
        recipient: &str,
        deadline: u64,
    ) -> Result<bool, BoxError> {
        let Some(reply) = self
            .directory
            .read::<Reply>(&key(scope, "replies", gate)?)
            .await?
        else {
            return Ok(false);
        };
        let reply = reply.value;
        if &reply.scope != scope
            || reply.gate_wake_ref != gate
            || reply.principal != recipient
            || reply.received_ms >= deadline
        {
            return Err("clarification reply correlation mismatch".into());
        }
        Ok(true)
    }
    pub(super) async fn save(&self, job: &Job) -> Result<(), BoxError> {
        let key = key(&job.scope, "jobs", &job.wake_ref)?;
        let old = self.directory.read::<Job>(&key).await?;
        self.directory
            .put(
                &key,
                job,
                old.map_or(PutMode::Create, |v| PutMode::Update(v.version)),
            )
            .await
    }
    async fn scope(&self) -> Result<RuntimeScope, BoxError> {
        let row = self
            .nexus
            .system_session()
            .read_control(DEFAULT_SPACE, "attention/config", None)
            .await?
            .ok_or("attention is not registered")?;
        Ok(
            serde_json::from_value::<anda_cognitive_nexus::attention::AttentionConfig>(row.value)?
                .scope,
        )
    }
    /// Trusted Rust host read. Public recipient-filtered inboxes use runtime_api.
    pub async fn status(&self, reference: &str) -> Result<Option<ActionStatus>, BoxError> {
        let scope = self.scope().await?;
        Ok(self.load(&scope, reference).await?.map(|j| j.status))
    }
    /// An answer is data, never an approval or a business execution permit.
    pub async fn respond(
        self: &Arc<Self>,
        reference: String,
        auth: AuthContext,
        response: ClarificationResponse,
    ) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                let scope = this.scope().await?;
                let session = this.session(&scope).await?;
                let job = this.confirm_parent(&session, &reference, &scope).await?;
                let p = job.prepared.as_ref().ok_or("clarification unavailable")?;
                if p.decision != "ask"
                    || p.recipient.as_deref() != Some(auth.principal_id.as_str())
                    || !principal_valid(&auth.principal_id)
                    || auth.auth_method.is_empty()
                    || auth.auth_strength == "none"
                    || !auth.delegation_chain.is_empty()
                    || response.event_key.is_empty()
                    || response.event_key.len() > 256
                    || response.answer.trim().is_empty()
                    || response.answer.len() > 16_384
                {
                    return Err("invalid authenticated clarification response".into());
                }
                // Recheck current visibility of the actual ask, even on replay.
                context::element(
                    &this.nexus.session(auth.clone()),
                    job.status
                        .decision_ref
                        .as_deref()
                        .ok_or("ask decision missing")?,
                    None,
                    this.bindings.limits.callbacks_ms,
                )
                .await?;
                let storage_key = key(&scope, "replies", &reference)?;
                if let Some(old) = this.directory.read::<Reply>(&storage_key).await? {
                    if old.value.scope == scope
                        && old.value.gate_wake_ref == reference
                        && old.value.principal == auth.principal_id
                        && old.value.response == response
                    {
                        return Ok(());
                    }
                    return Err("clarification response idempotency conflict".into());
                }
                if p.reply_deadline_ms
                    .is_none_or(|t| t <= anda_engine::unix_ms())
                {
                    return Err("clarification deadline passed; answer is not consent".into());
                }
                let reply = Reply {
                    scope,
                    gate_wake_ref: reference,
                    principal: auth.principal_id,
                    received_ms: anda_engine::unix_ms(),
                    response,
                };
                this.directory
                    .put(&storage_key, &reply, PutMode::Create)
                    .await
            })
            .await
    }
    /// Explicit operator retry of retained work. It does not bypass current
    /// native authority, dispatch lookup, generation, dependency or fence checks.
    pub async fn retry(self: &Arc<Self>, reference: String) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                if !this.automatic {
                    return Err("isolated host cannot retry external work".into());
                }
                let scope = this.scope().await?;
                let session = this.session(&scope).await?;
                let wake = session.read_wake(DEFAULT_SPACE, &reference).await?;
                if wake.pins != this.pins()
                    || matches!(
                        wake.state,
                        WakeState::Completed { .. } | WakeState::Cancelled { .. }
                    )
                {
                    return Err("work cannot be retried under this configuration/state".into());
                }
                let mut job = this
                    .load(&scope, &reference)
                    .await?
                    .ok_or("no retained action work")?;
                job.failures = 0;
                job.status.operator_retries = job
                    .status
                    .operator_retries
                    .checked_add(1)
                    .ok_or("operator retry counter exhausted")?;
                job.status.attempts = 0;
                job.rounds = 0;
                job.status.state = "pending".into();
                job.status.next_run_ms = None;
                this.save(&job).await
            })
            .await
    }

    pub(crate) async fn process(
        self: &Arc<Self>,
        wake: WakeRecord,
    ) -> Result<ActionStatus, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move { this.process_owned(wake).await })
            .await
    }

    async fn process_owned(&self, wake: WakeRecord) -> Result<ActionStatus, BoxError> {
        let _g = self.gate.lock().await;
        if !self.automatic {
            return Err("isolated host cannot run actions".into());
        }
        if !self.enabled().await? {
            return Ok(ActionStatus {
                wake_ref: wake.wake_ref,
                state: "disabled".into(),
                reason: Some("actions_disabled_by_host".into()),
                ..Default::default()
            });
        }
        if wake.pins != self.pins() {
            return Err(
                "action binding/policy pins changed; re-arm or operator migration required".into(),
            );
        }
        let mut job = self
            .load(&wake.scope, &wake.wake_ref)
            .await?
            .unwrap_or_else(|| Job::new(&wake, 0));
        if job.pins != wake.pins {
            return Err("action journal configuration mismatch".into());
        }
        if job
            .status
            .next_run_ms
            .is_some_and(|t| t > anda_engine::unix_ms())
        {
            return Ok(job.status);
        }
        if let Err(e) = self.drive(&wake, &mut job).await {
            job.failures = job.failures.saturating_add(1);
            job.status.reason = Some(e.to_string().chars().take(1024).collect());
            job.status.state = if job.failures > self.bindings.limits.max_retries {
                "blocked"
            } else {
                "retrying"
            }
            .into();
            job.status.next_run_ms = Some(
                anda_engine::unix_ms()
                    + self
                        .bindings
                        .limits
                        .retry_ms
                        .saturating_mul(1u64 << job.failures.min(6)),
            );
            // Retain failed prepared writes for exact receipt recovery.
            // Cancellation and failures never fabricate a silence decision.
            if job.status.state == "blocked" {
                job.status.next_run_ms = None;
                if let Ok(session) = self.session(&wake.scope).await
                    && let Ok(current) = session.read_wake(DEFAULT_SPACE, &wake.wake_ref).await
                    && matches!(&current.state,WakeState::Running{lease} if lease.owner==session.auth().principal_id && lease.expires_at_ms>anda_engine::unix_ms())
                {
                    let _ = self
                        .block(&session, &current, &mut job, "binding_unavailable")
                        .await;
                }
            }
        }
        self.save(&job).await?;
        Ok(job.status)
    }

    pub(super) async fn authorize(&self, request: &ActionRequest) -> Result<(), BoxError> {
        self.bounded(self.bindings.policy.authorize(request))
            .await?;
        self.bounded(self.executor(&request.kind)?.authorize(request))
            .await
    }
    pub(super) fn executor(&self, kind: &ActionKind) -> Result<&Arc<dyn ActionExecutor>, BoxError> {
        match kind {
            ActionKind::Business => self.bindings.business.as_ref(),
            ActionKind::DeliverClarification => {
                self.bindings.clarification.as_ref().map(|c| &c.executor)
            }
        }
        .ok_or_else(|| "action binding unavailable".into())
    }
    pub(super) async fn deduplicate(
        &self,
        session: &Session,
        request: &ActionRequest,
    ) -> Result<Option<String>, BoxError> {
        let Some(id) = &request.context.deduplication_key else {
            return Ok(None);
        };
        let key = key(&request.scope, "dedup", id)?;
        let digest = content_digest(
            &json!({"kind":request.kind,"payload":request.payload,"context":request.context}),
        )?;
        if let Some(old) = self.directory.read::<Dedup>(&key).await? {
            let old = old.value;
            if old.scope != request.scope
                || old.pins != request.pins
                || old.request_digest != digest
            {
                return Ok(Some("deduplication_identity_conflict".into()));
            }
            if old.owner == request.gate_wake_ref {
                return Ok(None);
            }
            let parent = self
                .confirm_parent(session, &old.owner, &request.scope)
                .await;
            let original = session.read_wake(DEFAULT_SPACE, &old.owner).await?;
            let source = context::element(
                session,
                &original.fire.watch_ref,
                None,
                self.bindings.limits.callbacks_ms,
            )
            .await?;
            if source["attributes"]["status"] != "fired"
                || source["facets"][format!("{PROFILE}WatchState")]["arm_generation"]
                    != original.fire.arm_generation
            {
                return Ok(Some(format!(
                    "deduplication_owner_generation_changed:{}",
                    old.owner
                )));
            }
            return Ok(Some(match parent {
                Ok(parent)
                    if parent
                        .prepared
                        .as_ref()
                        .is_some_and(|p| p.decision == "act") =>
                {
                    format!("duplicate_committed_operation:{}", old.owner)
                }
                _ => format!("deduplication_owner_unresolved:{}", old.owner),
            }));
        }
        self.directory
            .put(
                &key,
                &Dedup {
                    scope: request.scope.clone(),
                    pins: request.pins.clone(),
                    owner: request.gate_wake_ref.clone(),
                    request_digest: digest,
                },
                PutMode::Create,
            )
            .await?;
        Ok(None)
    }

    fn resume_condition(&self, wake: &WakeRecord) -> Json {
        json!({"format":FORMAT,"kind":"operator_or_dispatch_reconciled","scope":wake.scope,"wake":wake.wake_ref})
    }
    fn install_resume(&self, wake: &WakeRecord) -> Result<String, BoxError> {
        let condition = self.resume_condition(wake);
        let key = content_digest(&condition)?;
        let mut registered = self
            .resume_bindings
            .lock()
            .map_err(|_| "resume registration lock poisoned")?;
        if registered.contains(&key) {
            return Ok(key);
        }
        let key = self.nexus.register_wake_resume_verifier(
            condition,
            self.bindings.policy_pin.clone(),
            Arc::new(Resume {
                directory: self.directory.clone(),
                scope: wake.scope.clone(),
                reference: wake.wake_ref.clone(),
                nexus: Arc::downgrade(&self.nexus),
                timeout: Duration::from_millis(self.bindings.limits.callbacks_ms),
            }),
        )?;
        registered.insert(key.clone());
        Ok(key)
    }
    pub(super) async fn block(
        &self,
        session: &Session,
        wake: &WakeRecord,
        job: &mut Job,
        reason: &str,
    ) -> Result<(), BoxError> {
        let digest = self.install_resume(wake)?;
        job.status.state = "blocked".into();
        job.status.next_run_ms = None;
        self.save(job).await?;
        session
            .block_wake(
                DEFAULT_SPACE,
                &wake.wake_ref,
                wake.version,
                wake.fence,
                WakeRetry {
                    reason: reason.into(),
                    resume: WakeResume::OnChange {
                        condition_digest: digest,
                    },
                },
            )
            .await?;
        Ok(())
    }
    pub(super) async fn acquire(
        &self,
        session: &Session,
        wake: &mut WakeRecord,
        job: &Job,
    ) -> Result<(), BoxError> {
        if matches!(wake.state, WakeState::Blocked { .. }) {
            self.install_resume(wake)?;
            session
                .resume_wake(DEFAULT_SPACE, &wake.wake_ref, wake.version, wake.fence)
                .await?;
            *wake = session.read_wake(DEFAULT_SPACE, &wake.wake_ref).await?;
        }
        if matches!(&wake.state,WakeState::Running{lease} if lease.owner==session.auth().principal_id && lease.expires_at_ms>anda_engine::unix_ms()+self.bindings.limits.callbacks_ms*4)
        {
            return Ok(());
        }
        let expiry = crate::kip::timestamp(anda_engine::unix_ms() + self.bindings.limits.lease_ms);
        let result = if matches!(&wake.state,WakeState::Running{lease} if lease.owner==session.auth().principal_id && lease.expires_at_ms>anda_engine::unix_ms())
        {
            session
                .renew_wake(
                    DEFAULT_SPACE,
                    &wake.wake_ref,
                    wake.version,
                    wake.fence,
                    &expiry,
                )
                .await?
        } else {
            session
                .claim_wake(
                    DEFAULT_SPACE,
                    &wake.wake_ref,
                    wake.version,
                    wake.fence,
                    &expiry,
                )
                .await?
        };
        *wake = serde_json::from_value(result["wake"].clone())?;
        let _ = job;
        Ok(())
    }
}
