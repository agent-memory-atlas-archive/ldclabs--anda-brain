use super::*;
use anda_cognitive_nexus::{
    CognitiveNexus,
    attention::{AttentionConfig, RuntimePin, WakeResume, WakeState},
    nexus::DEFAULT_SPACE,
};
use anda_db::database::AndaDB;
use anda_kip::{Json, KipError};
use object_store::PutMode;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

/// The trusted per-Space boundary. Model arguments cannot install this object,
/// enable a scheduler, choose an executor, or supply an authorization context.
pub struct AttentionRuntime {
    id: String,
    db: Arc<AndaDB>,
    nexus: Arc<CognitiveNexus>,
    directory: Arc<Directory>,
    policy: AttentionPolicy,
    pub(super) automatic: bool,
    semantic: Option<Arc<super::semantic::SemanticRuntime>>,
    gate: Mutex<()>,
    tasks: crate::runtime::DurableTasks,
    running_until_ms: AtomicU64,
    actions: Option<Arc<crate::action::ActionRuntime>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BlockedWatch {
    version: u64,
    generation: u64,
    retry_at_ms: u64,
    reason: String,
}

struct BoundedResumeVerifier {
    inner: Arc<dyn anda_cognitive_nexus::attention::WakeResumeVerifier>,
    timeout: Duration,
}
#[async_trait::async_trait]
impl anda_cognitive_nexus::attention::WakeResumeVerifier for BoundedResumeVerifier {
    async fn verify(
        &self,
        input: anda_cognitive_nexus::attention::WakeResumeInput,
    ) -> Result<bool, KipError> {
        tokio::time::timeout(self.timeout, self.inner.verify(input))
            .await
            .map_err(|_| {
                KipError::new(
                    anda_kip::KipErrorCode::ExecutionTimeout,
                    "wake resume verifier exceeded its budget",
                )
            })?
    }
}

impl AttentionRuntime {
    /// Explicit trusted code installation; re-register after each process start.
    #[allow(clippy::result_large_err)] // Preserve the native structured host error.
    pub fn register_resume_verifier(
        &self,
        condition: Json,
        pin: RuntimePin,
        verifier: Arc<dyn anda_cognitive_nexus::attention::WakeResumeVerifier>,
    ) -> Result<String, KipError> {
        self.nexus.register_wake_resume_verifier(
            condition,
            pin,
            Arc::new(BoundedResumeVerifier {
                inner: verifier,
                timeout: Duration::from_millis(self.policy.wall_time_ms),
            }),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: String,
        db: Arc<AndaDB>,
        nexus: Arc<CognitiveNexus>,
        directory: Arc<Directory>,
        policy: AttentionPolicy,
        automatic: bool,
        bindings: Option<Arc<crate::action::ActionBindings>>,
        semantic: Option<super::semantic::SemanticBindings>,
    ) -> Arc<Self> {
        let actions = bindings.map(|b| {
            crate::action::ActionRuntime::new(
                nexus.clone(),
                directory.clone(),
                b,
                automatic,
                semantic
                    .as_ref()
                    .map(|s| s.config.pin().expect("validated semantic config")),
            )
        });
        Arc::new_cyclic(|owner| Self {
            semantic: semantic.map(|b| {
                super::semantic::SemanticRuntime::new(owner.clone(), directory.clone(), b)
            }),
            id,
            db,
            nexus,
            directory,
            policy,
            automatic,
            gate: Mutex::new(()),
            tasks: Default::default(),
            running_until_ms: AtomicU64::new(0),
            actions,
        })
    }
    pub fn is_busy(&self) -> bool {
        self.tasks.is_busy()
            || self.semantic.as_ref().is_some_and(|s| s.is_busy())
            || self.actions.as_ref().is_some_and(|a| a.is_busy())
            || self.running_until_ms.load(Ordering::SeqCst) > anda_engine::unix_ms()
    }
    pub(crate) async fn shutdown(&self) {
        if let Some(semantic) = &self.semantic {
            semantic.shutdown().await;
        }
        self.tasks.shutdown().await;
        if let Some(actions) = &self.actions {
            actions.shutdown().await;
        }
    }
    pub fn semantic(&self) -> Option<Arc<super::semantic::SemanticRuntime>> {
        self.semantic.clone()
    }
    pub async fn semantic_status(&self) -> super::semantic::SemanticStatus {
        match &self.semantic {
            Some(s) => s.status().await,
            None => super::semantic::SemanticStatus {
                reason: Some("semantic_evaluator_unavailable".into()),
                ..Default::default()
            },
        }
    }
    /// Trusted host inspection of the native configuration CAS version (not the
    /// Brain directory registration version). No public/model mutation route.
    pub async fn configuration(&self) -> Result<Option<(u64, AttentionConfig)>, BoxError> {
        self.nexus
            .system_session()
            .read_control(DEFAULT_SPACE, "attention/config", None)
            .await?
            .map(|row| Ok((row.version, serde_json::from_value(row.value)?)))
            .transpose()
    }
    /// Explicitly replace only the evaluator pin with the installed host binding.
    /// Existing arms retain their old basis and require operator-reviewed re-arm.
    /// This cannot migrate policy, binding, scope, work, or observed coverage.
    pub async fn reconfigure_evaluator(self: &Arc<Self>, expected: u64) -> Result<Json, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                let (version, mut config) = this
                    .configuration()
                    .await?
                    .ok_or("attention configuration missing")?;
                let pin = this.semantic.as_ref().map(|s| s.pin());
                if version != expected
                    && !(version == expected.saturating_add(1) && config.pins.evaluator == pin)
                {
                    return Err("attention configuration CAS changed".into());
                }
                let changed = config.pins.evaluator != pin;
                config.pins.evaluator = pin;
                if this
                    .actions
                    .as_ref()
                    .is_some_and(|a| a.pins() != config.pins)
                {
                    return Err("evaluator migration cannot change action policy/binding".into());
                }
                let enabled = this.status().await?.is_some_and(|s| s.registration.enabled)
                    && this.automatic
                    && this.policy.enabled;
                // Reconcile the durable directory before changing the native control.
                // An interrupted migration stays visibly blocked, never silently usable.
                this.directory.register(&this.id, &config, enabled).await?;
                let version = if changed {
                    let result = this
                        .nexus
                        .system_session()
                        .set_attention_config(DEFAULT_SPACE, expected, config.clone())
                        .await;
                    let (actual, saved) = this
                        .configuration()
                        .await?
                        .ok_or("attention configuration missing")?;
                    if actual != expected.saturating_add(1) || saved != config {
                        return Err(result
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "attention migration readback mismatch".into())
                            .into());
                    }
                    actual
                } else {
                    version
                };
                Ok(json!({"version":version,"config":config,"watches_require_rearm":true}))
            })
            .await
    }
    pub fn actions(&self) -> Option<Arc<crate::action::ActionRuntime>> {
        self.actions.clone()
    }
    /// Configuration/capability discovery only; this does not claim a wake or
    /// infer current target-system authorization for any particular action.
    pub async fn runtime_status(&self) -> Result<Json, BoxError> {
        let status = self.status().await?;
        let enabled = self.automatic
            && self.policy.enabled
            && status.as_ref().is_some_and(|s| s.registration.enabled);
        let actions_enabled = match &self.actions {
            Some(a) if status.is_some() => a.enabled().await?,
            _ => false,
        };
        let semantic = self.semantic_status().await;
        Ok(
            json!({"semantic_attention":semantic,"structured_attention":{"supported":true,"enabled":enabled,"registered":status.is_some()},
            "actions":{"supported":true,"configured":self.actions.is_some(),"enabled":enabled && actions_enabled,
                "ready":null,"readiness":"validated_per_work_item","pins":self.actions.as_ref().map(|a|a.pins()),
                "blocked_reason":if self.actions.is_none(){Some("action_bindings_not_installed")}else{None}},
            "last_scan":status.as_ref().map(|s|&s.last_report)}),
        )
    }
    pub(super) async fn work_session(
        &self,
    ) -> Result<anda_cognitive_nexus::nexus::Session, BoxError> {
        match &self.actions {
            Some(actions) => {
                actions
                    .session(
                        &self
                            .config()
                            .await?
                            .ok_or("attention configuration missing")?
                            .scope,
                    )
                    .await
            }
            None => Ok(match &self.semantic {
                Some(s) => {
                    let mut auth =
                        anda_cognitive_nexus::governance::AuthContext::principal(s.principal());
                    auth.auth_method = "brain:registered-semantic-controller".into();
                    self.nexus.session(auth)
                }
                None => self.nexus.system_session(),
            }),
        }
    }
    pub(super) async fn config(&self) -> Result<Option<AttentionConfig>, BoxError> {
        self.nexus
            .system_session()
            .read_control(DEFAULT_SPACE, "attention/config", None)
            .await?
            .map(|r| serde_json::from_value(r.value).map_err(Into::into))
            .transpose()
    }
    async fn register_inner(&self) -> Result<Entry, BoxError> {
        let native = self.config().await?;
        let saved: Option<String> = self.db.get_extension_as("attention_instance");
        let instance = native
            .as_ref()
            .map(|c| c.scope.space_instance.clone())
            .or(saved.clone())
            .unwrap_or(anda_cognitive_nexus::content_digest(
                &json!({"nonce":rand::random::<[u8;32]>()}),
            )?);
        if saved.as_ref().is_some_and(|v| v != &instance) {
            return Err("attention database instance mismatch".into());
        }
        if saved.is_none() {
            self.db
                .save_extension_from("attention_instance".into(), &instance)
                .await?;
        }
        let mut config = native.clone().unwrap_or(AttentionConfig {
            scope: RuntimeScope {
                space_id: self.id.clone(),
                space_instance: instance,
            },
            pins: RuntimePins {
                policy: RuntimePin {
                    id: "nexus:structured-watch-v1".into(),
                    digest: anda_cognitive_nexus::content_digest(
                        &json!({"engine":"nexus:structured-watch-v1"}),
                    )?,
                },
                evaluator: None,
                binding: None,
            },
        });
        if let Some(actions) = &self.actions {
            if native.is_none() {
                config.pins = actions.pins();
            }
            if config.pins != actions.pins() {
                return Err("action configuration changed; explicit native reconfiguration and Watch re-arm required".into());
            }
            if let Some(semantic) = &self.semantic
                && actions.session(&config.scope).await?.auth().principal_id != semantic.principal()
            {
                return Err("semantic controller must match the action/arming principal".into());
            }
            actions.install().await?;
        }
        if let Some(semantic) = &self.semantic {
            if native.is_none() {
                config.pins.evaluator = Some(semantic.pin());
            }
            if config.pins.evaluator != Some(semantic.pin()) {
                return Err("semantic evaluator changed; explicit native reconfiguration and Watch re-arm required".into());
            }
        }
        // An index/registration ACK is required before the first native write.
        let entry = self
            .directory
            .register(&self.id, &config, self.automatic && self.policy.enabled)
            .await?;
        if native.is_none() {
            self.nexus
                .system_session()
                .set_attention_config(DEFAULT_SPACE, 0, config)
                .await?;
        }
        Ok(entry)
    }
    /// Explicit contract for trusted hosts that write directly through Nexus:
    /// call and await this BEFORE arming/enrolling new work, never after it.
    pub async fn register_work(self: &Arc<Self>) -> Result<Registration, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                Ok(this.register_inner().await?.registration)
            })
            .await
    }
    pub(crate) async fn discover_existing(self: &Arc<Self>) -> Result<(), BoxError> {
        if self.directory.entry(&self.id).await?.is_none() && self.config().await?.is_some() {
            self.register_work().await?;
        }
        Ok(())
    }
    pub async fn status(&self) -> Result<Option<AttentionStatus>, BoxError> {
        Ok(self
            .directory
            .entry(&self.id)
            .await?
            .map(|e| e.value.status()))
    }
    pub async fn set_enabled(self: &Arc<Self>, enabled: bool) -> Result<(), BoxError> {
        if enabled && !self.automatic {
            return Err("isolated hosts cannot enable automatic attention".into());
        }
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.register_inner().await?;
                this.directory
                    .edit(&this.id, |e| {
                        e.registration.enabled = enabled;
                        Ok(())
                    })
                    .await
            })
            .await
    }
    pub async fn arm_watch(
        self: &Arc<Self>,
        target: String,
        expected: u64,
    ) -> Result<Json, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.register_inner().await?;
                Ok(this
                    .work_session()
                    .await?
                    .arm_watch(DEFAULT_SPACE, &target, expected)
                    .await?)
            })
            .await
    }
    /// Provision only the cancellation of this newly created product Watch.
    /// The durable marker prevents a retry/restart from regranting revoked rights.
    pub(crate) async fn provision_watch_cancellation(
        self: &Arc<Self>,
        target: String,
    ) -> Result<(), BoxError> {
        use anda_cognitive_nexus::governance::{rows::AuthorityScope, store::GrantDraft};
        let this = self.clone();
        self.tasks.run(async move {
            let _guard=this.gate.lock().await;
            let session=this.work_session().await?;
            let principal=session.auth().principal_id.clone();
            let key=format!("watch-cancellation/{}",&anda_cognitive_nexus::content_digest(&json!({"watch":target,"principal":principal}))?[7..]);
            if let Some(marker)=this.directory.read::<bool>(&key).await? {
                if marker.value {return Ok(())}
                let grants=this.nexus.governance().grants_for(DEFAULT_SPACE,&principal,&[]).await?;
                if grants.iter().any(|grant|grant.scope["elements"]==json!([target])&&grant.actions==vec!["archive".to_string()]) {
                    this.directory.put(&key,&true,object_store::PutMode::Update(marker.version)).await?;
                    return Ok(())
                }
                return Err("Watch cancellation provisioning interrupted; explicit grant review required".into())
            }
            this.directory.put(&key,&false,object_store::PutMode::Create).await?;
            this.nexus.system_session().create_grant(DEFAULT_SPACE,GrantDraft {grantee_principal:principal,actions:vec!["archive".into()],scope:AuthorityScope {elements:vec![target],..Default::default()},..Default::default()}).await?;
            let marker=this.directory.read::<bool>(&key).await?.ok_or("watch grant marker missing")?;
            this.directory.put(&key,&true,object_store::PutMode::Update(marker.version)).await?;
            Ok(())
        }).await
    }
    /// Trusted controller cancellation; never grants the requester authority.
    pub async fn archive_watch(
        self: &Arc<Self>,
        target: String,
        expected: u64,
    ) -> Result<Json, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _guard = this.gate.lock().await;
                this.register_inner().await?;
                let request = crate::kip::request_with(
                    "TRANSITION :target TO \"archived\" EXPECT VERSION :version",
                    serde_json::Map::from_iter([
                        ("target".into(), serde_json::json!(target)),
                        ("version".into(), serde_json::json!(expected)),
                    ]),
                );
                let response =
                    anda_kip::execute_request(&this.work_session().await?, &request).await;
                crate::kip::ok_result(&response)
                    .cloned()
                    .ok_or_else(|| crate::kip::error_message(&response).into())
            })
            .await
    }

    /// Only a trusted consumer supplies these obligations. A due time requests
    /// revalidation; it never changes Assertion truth or authorizes an action.
    pub async fn schedule_recheck(self: &Arc<Self>, mut recheck: Recheck) -> Result<(), BoxError> {
        if recheck.key.is_empty() || recheck.key.len() > 256 || recheck.due_at_ms > MAX_COUNTER {
            return Err("invalid bounded recheck".into());
        }
        recheck.notified = false;
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                if let Some(old) = this.directory.entry(&this.id).await?
                    && old.value.rechecks.iter().any(|r| {
                        r.key == recheck.key
                            && r.source == recheck.source
                            && r.due_at_ms == recheck.due_at_ms
                    })
                {
                    return Ok(());
                }
                this.register_inner().await?;
                this.directory
                    .edit(&this.id, |entry| {
                        if let Some(old) = entry.rechecks.iter_mut().find(|r| r.key == recheck.key)
                        {
                            *old = recheck;
                        } else {
                            if entry.rechecks.len() >= 128 {
                                return Err("recheck queue is full".into());
                            }
                            entry.rechecks.push(recheck);
                        }
                        Ok(())
                    })
                    .await
            })
            .await
    }
    pub async fn acknowledge_recheck(
        self: &Arc<Self>,
        key: String,
        due_at_ms: u64,
    ) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.directory
                    .edit(&this.id, |entry| {
                        let index = entry
                            .rechecks
                            .iter()
                            .position(|r| r.key == key && r.due_at_ms == due_at_ms)
                            .ok_or("recheck changed or unavailable")?;
                        entry.rechecks.remove(index);
                        Ok(())
                    })
                    .await
            })
            .await
    }
    /// Schedule native temporal invalidation without accepting a timestamp
    /// asserted in cognitive content. Callers pass the typed native basis.
    pub async fn observe_basis(
        self: &Arc<Self>,
        basis: &anda_kip::ProjectionBasis,
    ) -> Result<(), BoxError> {
        let this = self.clone();
        let basis = basis.clone();
        self.tasks
            .run(async move { this.observe_basis_inner(&basis).await })
            .await
    }
    pub(crate) fn notice_read(
        self: &Arc<Self>,
        request: &anda_kip::Request,
        response: &anda_kip::Response,
    ) {
        if !self.automatic || !self.policy.enabled {
            return;
        }
        let Some(basis) = super::expiry::earliest_basis(request, response) else {
            return;
        };
        let this = self.clone();
        if let Err(err) = self.tasks.start(async move {
            if let Err(err) = this.observe_basis_inner(&basis).await {
                log::warn!(target: "brain", space_id = this.id; "projection recheck hint was not retained: {err}");
            }
            Ok(())
        }) { log::warn!(target: "brain", "projection recheck queue unavailable: {err}"); }
    }
    async fn observe_basis_inner(&self, basis: &anda_kip::ProjectionBasis) -> Result<(), BoxError> {
        let _g = self.gate.lock().await;
        if basis.space_id != DEFAULT_SPACE {
            return Err("projection basis space mismatch".into());
        }
        let Some(at) = &basis.next_invalid_at else {
            return Ok(());
        };
        let due = time_ms(at)?;
        // One conservative earliest-expiry obligation; do not fill the catalog
        // with one timer for each repeated Recall of the same basis.
        let Some(status) = self.status().await? else {
            return Ok(());
        };
        if status
            .rechecks
            .iter()
            .any(|r| r.key == "native_projection_expiry" && !r.notified && r.due_at_ms <= due)
        {
            return Ok(());
        }
        let recheck = Recheck {
            key: "native_projection_expiry".into(),
            source: RecheckSource::DependencyInvalidation,
            due_at_ms: due,
            notified: false,
        };
        // This off-graph hint cannot initialize or mutate native controls from
        // a read path. Registered work is independently reconciled after restart.
        self.directory
            .edit(&self.id, |entry| {
                if let Some(old) = entry.rechecks.iter_mut().find(|r| r.key == recheck.key) {
                    *old = recheck;
                } else {
                    if entry.rechecks.len() >= 128 {
                        return Err("recheck queue is full".into());
                    }
                    entry.rechecks.push(recheck);
                }
                Ok(())
            })
            .await
    }
    async fn watch_page(
        &self,
        after: &str,
        runnable: bool,
        limit: usize,
    ) -> Result<Vec<crate::settlement::watch::WatchRow>, BoxError> {
        let filters = if runnable {
            r#"FILTER(IS_NOT_NULL(?w.facets["WatchState"].arm_generation)) FILTER(IS_NULL(?w.attributes.condition.text)) FILTER(IS_NOT_NULL(?w.attributes.condition.element) || IS_NOT_NULL(?w.attributes.condition.slot) || IS_NOT_NULL(?w.attributes.condition.type))"#
        } else {
            ""
        };
        let request = crate::kip::request_with(
            format!(
                r#"FIND(?w.id, ?w.name, ?w.attributes, ?w._system.version, ?w.facets["WatchState"], ?w.schema_ref)
WHERE {{?w CONCEPT {{type:"Watch"}} FILTER(?w.attributes.status == "armed") FILTER(?w.id > :after) {filters}}}
ORDER BY ?w.id LIMIT {limit}"#
            ),
            crate::kip::param("after", after),
        );
        let response = tokio::time::timeout(
            crate::agents::READONLY_KIP_TIMEOUT,
            crate::kip::execute_readonly_request(self.nexus.as_ref(), &request),
        )
        .await?;
        let result =
            crate::kip::ok_result(&response).ok_or_else(|| crate::kip::error_message(&response))?;
        Ok(crate::settlement::watch::read_watch_rows(result))
    }
    fn blocked_key(&self, id: &str) -> Result<String, BoxError> {
        Ok(format!(
            "blocked/{}/{}",
            &anda_cognitive_nexus::content_digest(&json!(self.id))?[7..],
            id
        ))
    }
    #[allow(clippy::result_large_err)] // RunKip preserves native error codes and details.
    pub(crate) async fn advance(
        self: &Arc<Self>,
        id: String,
        version: u64,
        generation: u64,
    ) -> Result<Json, KipError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.register_inner().await?;
                Ok(this
                    .work_session()
                    .await?
                    .advance_watch(
                        DEFAULT_SPACE,
                        &id,
                        version,
                        generation,
                        this.policy.changes_per_watch,
                    )
                    .await?)
            })
            .await
            .map_err(|e| match e.downcast::<KipError>() {
                Ok(e) => *e,
                Err(e) => KipError::internal_error(e.to_string()),
            })
    }
    pub async fn tick(self: &Arc<Self>) -> Result<AttentionReport, BoxError> {
        self.scan(Instant::now() + Duration::from_millis(self.policy.wall_time_ms))
            .await
    }
    pub(crate) async fn scan(
        self: &Arc<Self>,
        deadline: Instant,
    ) -> Result<AttentionReport, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                let Some(mut entry) = this.directory.entry(&this.id).await?.map(|e| e.value) else {
                    return Ok(AttentionReport {
                        scan_complete: true,
                        ..Default::default()
                    });
                };
                let report = match this.scan_inner(&mut entry, deadline).await {
                    Ok(report) => {
                        entry.failures = 0;
                        report
                    }
                    Err(e) => {
                        entry.failures = entry.failures.saturating_add(1);
                        entry.registration.next_check_ms = anda_engine::unix_ms()
                            + (this
                                .policy
                                .tick_ms
                                .saturating_mul(1 << entry.failures.min(8)))
                            .min(this.policy.blocked_retry_ms);
                        AttentionReport {
                            error: Some(e.to_string()),
                            ..Default::default()
                        }
                    }
                };
                entry.last_report = report.clone();
                this.directory.finish(entry).await?;
                Ok(report)
            })
            .await
    }
    async fn scan_inner(
        &self,
        entry: &mut Entry,
        deadline: Instant,
    ) -> Result<AttentionReport, BoxError> {
        let mut report = AttentionReport::default();
        if !self.policy.enabled || !entry.registration.enabled {
            report.defer("attention_disabled", true);
            return Ok(report);
        }
        let cfg = self
            .config()
            .await?
            .ok_or("registered native attention configuration is missing")?;
        if cfg.scope != entry.native_scope || cfg.pins != entry.registration.pins {
            return Err(
                "attention configuration changed; registered host reconciliation required".into(),
            );
        }
        let generation = entry.registration.dirty_generation;
        if entry.checkpoint.generation != generation
            || (entry.checkpoint.runnable_done
                && entry.checkpoint.all_done
                && entry.checkpoint.wakes_done)
        {
            entry.checkpoint = Checkpoint {
                generation,
                ..Default::default()
            };
        }
        let now = anda_engine::unix_ms();
        for recheck in &mut entry.rechecks {
            if recheck.due_at_ms <= now {
                recheck.notified = true;
                report.rechecks_due += 1;
            } else {
                min_due(&mut entry.checkpoint.next_due_ms, recheck.due_at_ms);
            }
        }
        // Runnable watches have their own cursor: a large text-only population
        // cannot delay a structured deadline behind its diagnostic scan.
        if Instant::now() < deadline {
            let rows = self
                .watch_page(
                    &entry.checkpoint.runnable_after,
                    true,
                    self.policy.watches_per_space,
                )
                .await?;
            let short = rows.len() < self.policy.watches_per_space;
            let mut exhausted = false;
            for row in rows {
                if Instant::now() >= deadline {
                    exhausted = true;
                    break;
                }
                report.watches_scanned += 1;
                let key = self.blocked_key(&row.id)?;
                let old = self.directory.read::<BlockedWatch>(&key).await?;
                let generation = row.generation.ok_or("runnable Watch lacks generation")?;
                let retry = old.as_ref().is_some_and(|b| {
                    b.value.version == row.version
                        && b.value.generation == generation
                        && b.value.retry_at_ms > now
                });
                if retry {
                    report.defer("watch_retry_backoff", false);
                } else {
                    match self
                        .work_session()
                        .await?
                        .advance_watch(
                            DEFAULT_SPACE,
                            &row.id,
                            row.version,
                            generation,
                            self.policy.changes_per_watch,
                        )
                        .await
                    {
                        Ok(result) => {
                            report.advanced += 1;
                            match result["status"].as_str() {
                                Some("fired") => report.fired += 1,
                                Some("expired") => report.expired += 1,
                                _ => {}
                            }
                        }
                        Err(e) => {
                            report.defer("native_watch_unavailable", false);
                            if e.code == anda_kip::KipErrorCode::VersionConflict {
                                report.conflicted += 1;
                            }
                            report.error = Some(e.to_string());
                            let blocked = BlockedWatch {
                                version: row.version,
                                generation,
                                retry_at_ms: now + self.policy.blocked_retry_ms,
                                reason: e.to_string().chars().take(1024).collect(),
                            };
                            self.directory
                                .put(
                                    &key,
                                    &blocked,
                                    old.map_or(PutMode::Create, |v| PutMode::Update(v.version)),
                                )
                                .await?;
                        }
                    }
                }
                entry.checkpoint.runnable_after = row.id;
                min_due(&mut entry.checkpoint.next_due_ms, now + self.policy.tick_ms);
            }
            if short && !exhausted {
                entry.checkpoint.runnable_done = true;
                entry.checkpoint.runnable_after.clear();
            }
        }
        if !entry.checkpoint.all_done && Instant::now() < deadline {
            let rows = self
                .watch_page(
                    &entry.checkpoint.all_after,
                    false,
                    self.policy.watches_per_space,
                )
                .await?;
            entry.checkpoint.all_done = rows.len() < self.policy.watches_per_space;
            for row in rows {
                if !crate::settlement::watch::is_structured(&row.condition)
                    || row.generation.is_none()
                {
                    report.defer(
                        if row.generation.is_none() {
                            "legacy_watch_requires_replacement"
                        } else if self.semantic.is_some() {
                            "semantic_evaluation_separate_pass"
                        } else {
                            "semantic_evaluator_unavailable"
                        },
                        false,
                    );
                }
                if self.semantic.is_some()
                    && !crate::settlement::watch::is_structured(&row.condition)
                {
                    min_due(&mut entry.checkpoint.next_due_ms, now + self.policy.tick_ms);
                }
                if let Ok(due) = time_ms(&row.watch.due_at)
                    && due > now
                {
                    min_due(&mut entry.checkpoint.next_due_ms, due);
                }
                entry.checkpoint.all_after = row.id;
            }
        }
        if !entry.checkpoint.wakes_done && Instant::now() < deadline {
            let session = self.nexus.system_session();
            if entry.checkpoint.wake_pending.is_empty() {
                let page = match session
                    .list_wakes(
                        DEFAULT_SPACE,
                        entry.checkpoint.wake_cursor.as_deref(),
                        self.policy.wakes_per_space,
                    )
                    .await
                {
                    Ok(page) => page,
                    Err(e) if e.code == anda_kip::KipErrorCode::VersionConflict => {
                        entry.checkpoint.wake_cursor = None;
                        return Err(e.into());
                    }
                    Err(e) => return Err(e.into()),
                };
                report.wakes_scanned = page.scanned;
                entry.checkpoint.wake_pending =
                    page.items.into_iter().map(|w| w.wake_ref).collect();
                entry.checkpoint.wake_page_cursor = page.next_cursor;
                entry.checkpoint.wake_page_complete = page.complete;
            }
            while let Some(reference) = entry.checkpoint.wake_pending.first().cloned() {
                if Instant::now() >= deadline {
                    break;
                }
                if self
                    .actions
                    .as_ref()
                    .is_some_and(|a| report.actions_processed >= a.per_pass())
                {
                    report.budget_exhausted = true;
                    break;
                }
                self.process_wake(entry, &reference, &mut report).await?;
                entry.checkpoint.wake_pending.remove(0);
            }
            if entry.checkpoint.wake_pending.is_empty() {
                entry.checkpoint.wake_cursor = entry.checkpoint.wake_page_cursor.take();
                entry.checkpoint.wakes_done = entry.checkpoint.wake_page_complete;
            }
            self.running_until_ms
                .fetch_max(entry.checkpoint.running_until_ms, Ordering::SeqCst);
        }
        report.scan_complete = entry.checkpoint.runnable_done
            && entry.checkpoint.all_done
            && entry.checkpoint.wakes_done;
        report.budget_exhausted = !report.scan_complete || Instant::now() >= deadline;
        if report.scan_complete {
            entry.last_scan_ms = now;
        }
        entry.registration.next_check_ms = if report.scan_complete {
            entry
                .checkpoint
                .next_due_ms
                .unwrap_or(now + self.policy.reconcile_ms)
                .min(now + self.policy.reconcile_ms)
        } else {
            now + self.policy.tick_ms
        };
        if report.rechecks_due > 0 {
            report
                .error
                .get_or_insert_with(|| "recheck retained for its trusted consumer".into());
        }
        Ok(report)
    }
    async fn process_wake(
        &self,
        entry: &mut Entry,
        reference: &str,
        report: &mut AttentionReport,
    ) -> Result<(), BoxError> {
        let session = self.nexus.system_session();
        let wake = session.read_wake(DEFAULT_SPACE, reference).await?;
        let now = anda_engine::unix_ms();
        if let Some(actions) = &self.actions
            && !matches!(
                wake.state,
                WakeState::Completed { .. } | WakeState::Cancelled { .. }
            )
            && !matches!(wake.state, WakeState::Pending { not_before_ms } if not_before_ms > now)
        {
            if report.actions_processed >= actions.per_pass() {
                report.budget_exhausted = true;
                min_due(&mut entry.checkpoint.next_due_ms, now + self.policy.tick_ms);
                return Ok(());
            }
            report.actions_processed += 1;
            match actions.process(wake).await {
                Ok(status) => {
                    *report
                        .action_states
                        .entry(status.state.clone())
                        .or_default() += 1;
                    if status.reason.is_some() {
                        report.defer("action_blocked", true);
                    }
                    min_due(
                        &mut entry.checkpoint.next_due_ms,
                        status.next_run_ms.unwrap_or(now + self.policy.reconcile_ms),
                    );
                }
                Err(e) => {
                    report.defer("action_configuration_or_source_unavailable", true);
                    report.error = Some(e.to_string());
                    min_due(
                        &mut entry.checkpoint.next_due_ms,
                        now + self.policy.blocked_retry_ms,
                    );
                }
            }
            if let Ok(current) = session.read_wake(DEFAULT_SPACE, reference).await
                && let WakeState::Running { lease } = current.state
            {
                entry.checkpoint.running_until_ms =
                    entry.checkpoint.running_until_ms.max(lease.expires_at_ms);
            }
            return Ok(());
        }
        match wake.state {
            WakeState::Pending { not_before_ms } => {
                report.pending += 1;
                if self.actions.is_none() {
                    report.defer("action_bindings_not_installed", true);
                }
                min_due(
                    &mut entry.checkpoint.next_due_ms,
                    not_before_ms.max(now + self.policy.tick_ms),
                );
            }
            WakeState::Running { lease } => {
                entry.checkpoint.running_until_ms =
                    entry.checkpoint.running_until_ms.max(lease.expires_at_ms);
                min_due(
                    &mut entry.checkpoint.next_due_ms,
                    lease.expires_at_ms.max(now + self.policy.tick_ms),
                );
            }
            WakeState::Blocked { retry } => {
                let due = match retry.resume {
                    WakeResume::At { not_before_ms } => not_before_ms,
                    WakeResume::OnChange { .. } => now,
                };
                let key = self.blocked_key(reference)?;
                let old = self.directory.read::<BlockedWatch>(&key).await?;
                let backoff = old
                    .as_ref()
                    .filter(|b| b.value.version == wake.version && b.value.generation == wake.fence)
                    .map_or(0, |b| b.value.retry_at_ms);
                let mut next_try = due.max(backoff);
                if due <= now && backoff <= now {
                    match session
                        .resume_wake(DEFAULT_SPACE, reference, wake.version, wake.fence)
                        .await
                    {
                        Ok(_) => {
                            report.resumed += 1;
                            next_try = now + self.policy.tick_ms;
                        }
                        Err(error) => {
                            report.defer("wake_resume_unverified", true);
                            next_try = now + self.policy.blocked_retry_ms;
                            let blocked = BlockedWatch {
                                version: wake.version,
                                generation: wake.fence,
                                retry_at_ms: now + self.policy.blocked_retry_ms,
                                reason: error.to_string().chars().take(1024).collect(),
                            };
                            self.directory
                                .put(
                                    &key,
                                    &blocked,
                                    old.map_or(PutMode::Create, |v| PutMode::Update(v.version)),
                                )
                                .await?;
                        }
                    }
                } else if backoff > now {
                    report.defer("wake_resume_backoff", true);
                }
                min_due(
                    &mut entry.checkpoint.next_due_ms,
                    next_try.max(now + self.policy.tick_ms),
                );
            }
            _ => {}
        }
        Ok(())
    }
}
