use super::*;
use crate::attention::{AttentionRuntime, Directory};
use anda_cognitive_nexus::{
    attention::{AttentionConfig, WatchEvaluation},
    content_digest,
    nexus::{DEFAULT_SPACE, Session},
};
use anda_kip::cognitive::ArtifactPin;
use futures::FutureExt;
use object_store::PutMode;
use std::{
    sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

pub struct SemanticRuntime {
    parent: Weak<AttentionRuntime>,
    directory: Arc<Directory>,
    bindings: SemanticBindings,
    pin: RuntimePin,
    tasks: crate::runtime::DurableTasks,
    gate: Mutex<()>,
    running: AtomicBool,
    last: parking_lot::RwLock<SemanticPass>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    after: String,
    /// Persisted before preparation, including a Watch that may become terminal
    /// before the host sees the native commit ACK.
    pending: Option<String>,
    last: SemanticPass,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    watch: String,
    version: u64,
    generation: u64,
    epoch: u64,
    attempts: u32,
    ticket: Option<String>,
    evaluation: Option<ArtifactPin>,
    accepted: bool,
    progress: SemanticProgress,
    retry_at_ms: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticProgress {
    pub ticket_ref: Option<String>,
    pub evaluation_ref: Option<String>,
    pub status: Option<String>,
    pub reason: Option<String>,
}
impl SemanticRuntime {
    pub(crate) fn new(
        parent: Weak<AttentionRuntime>,
        directory: Arc<Directory>,
        bindings: SemanticBindings,
    ) -> Arc<Self> {
        let pin = bindings.config.pin().expect("validated semantic bindings");
        Arc::new(Self {
            parent,
            directory,
            bindings,
            pin,
            tasks: Default::default(),
            gate: Mutex::new(()),
            running: AtomicBool::new(false),
            last: Default::default(),
        })
    }
    pub fn pin(&self) -> RuntimePin {
        self.pin.clone()
    }
    pub(crate) fn principal(&self) -> &str {
        &self.bindings.config.principal
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.tasks.is_busy()
    }
    pub(crate) async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }
    pub async fn status(&self) -> SemanticStatus {
        let mut status = SemanticStatus {
            configured: true,
            pin: Some(self.pin()),
            running: self.is_busy(),
            last_pass: Some(self.last.read().clone()),
            ..Default::default()
        };
        match self.context().await {
            Ok((_, config, automatic)) => {
                status.automatic = automatic && self.bindings.config.automatic;
                if let Ok(key) = self.key(&config, "cursor") {
                    match self.directory.read::<Cursor>(&key).await {
                        Ok(Some(cursor)) => {
                            if !status.running
                                && status.last_pass.as_ref().is_none_or(|p| p.reason.is_none())
                            {
                                status.last_pass = Some(cursor.value.last);
                            }
                            if cursor.value.pending.is_some() && !self.is_busy() {
                                status.reason =
                                    Some("semantic_processing_requires_recovery".into());
                            }
                        }
                        Err(_) => status.reason = Some("semantic_journal_unavailable".into()),
                        _ => {}
                    }
                }
            }
            Err(_) => {
                status.reason = Some("semantic_configuration_or_authority_unavailable".into())
            }
        }
        if status.reason.is_none() {
            status.reason = status.last_pass.as_ref().and_then(|p| p.reason.clone());
        }
        status
    }
    async fn context(&self) -> Result<(Session, AttentionConfig, bool), BoxError> {
        let parent = self.parent.upgrade().ok_or("semantic owner closed")?;
        let config = parent
            .config()
            .await?
            .ok_or("attention configuration missing")?;
        if config.pins.evaluator.as_ref() != Some(&self.pin) {
            return Err("semantic evaluator changed; explicit native reconfiguration and Watch re-arm required".into());
        }
        let registration = parent
            .status()
            .await?
            .ok_or("attention registration missing")?;
        if !registration.registration.enabled || registration.registration.pins != config.pins {
            return Err("semantic attention disabled or registration changed".into());
        }
        let session = parent.work_session().await?;
        if session.auth().principal_id != self.bindings.config.principal {
            return Err("semantic controller differs from arming principal".into());
        }
        // Check current authority before exposing any event data to the model.
        let authority = session.effective_authority(DEFAULT_SPACE).await?;
        for permission in [
            anda_cognitive_nexus::governance::Permission::ReadHistory,
            anda_cognitive_nexus::governance::Permission::Derive,
        ] {
            authority
                .authorize(permission, &Default::default(), session.auth())
                .into_result()?;
        }
        Ok((session, config, parent.automatic))
    }
    fn key(&self, config: &AttentionConfig, suffix: &str) -> Result<String, BoxError> {
        Ok(format!(
            "semantic/{}/{suffix}",
            &content_digest(&json!({"scope":config.scope,"pin":self.pin}))?[7..]
        ))
    }
    async fn save<T: Serialize + for<'de> Deserialize<'de>>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), BoxError> {
        let old = self.directory.read::<T>(key).await?;
        self.directory
            .put(
                key,
                value,
                old.map_or(PutMode::Create, |v| PutMode::Update(v.version)),
            )
            .await
    }
    /// Trusted, explicit bounded pass; no model/tool or public request exposes it.
    /// Attention registration must be enabled; automatic model calls can remain off.
    pub async fn run_once(self: &Arc<Self>) -> Result<SemanticPass, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                let _slot = this
                    .directory
                    .semantic_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| "semantic host concurrency budget exhausted")?;
                let result = this.pass().await;
                match &result {
                    Ok(pass) => *this.last.write() = pass.clone(),
                    Err(_) => {
                        this.last.write().reason =
                            Some("semantic_processing_requires_recovery".into())
                    }
                }
                result
            })
            .await
    }
    pub(crate) fn kick(self: &Arc<Self>) {
        if !self.bindings.config.automatic
            || !self.parent.upgrade().is_some_and(|p| p.automatic)
            || self.running.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let Ok(slot) = self.directory.semantic_slots.clone().try_acquire_owned() else {
            self.running.store(false, Ordering::SeqCst);
            return;
        };
        let this = self.clone();
        if self
            .tasks
            .start(async move {
                let _slot = slot;
                let _g = this.gate.lock().await;
                let result = std::panic::AssertUnwindSafe(this.pass())
                    .catch_unwind()
                    .await;
                match result {
                    Ok(Ok(pass)) => *this.last.write() = pass,
                    _ => {
                        this.last.write().reason =
                            Some("semantic_processing_requires_recovery".into())
                    }
                }
                this.running.store(false, Ordering::SeqCst);
                Ok(())
            })
            .is_err()
        {
            self.running.store(false, Ordering::SeqCst);
        }
    }
    pub async fn progress(&self, watch: &str) -> Result<Option<SemanticProgress>, BoxError> {
        let (_, config, _) = self.context().await?;
        if !watch.starts_with("C-") || watch[2..].parse::<u64>().is_err() {
            return Err("invalid semantic Watch ID".into());
        }
        Ok(self
            .directory
            .read::<Job>(&self.key(&config, &format!("jobs/{watch}"))?)
            .await?
            .map(|j| j.value.progress))
    }
    /// Explicit operator retry after the fixed attempt budget. No re-arm, new
    /// coverage interval, or evaluator substitution is performed.
    pub async fn retry(self: &Arc<Self>, watch: String) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                let (_, config, _) = this.context().await?;
                if !watch.starts_with("C-") || watch[2..].parse::<u64>().is_err() {
                    return Err("invalid semantic Watch ID".into());
                }
                let key = this.key(&config, &format!("jobs/{watch}"))?;
                let mut job = this
                    .directory
                    .read::<Job>(&key)
                    .await?
                    .ok_or("semantic work not found")?
                    .value;
                if job.evaluation.is_some() || job.accepted {
                    return Err("resolve pending/accepted semantic commit before retry".into());
                }
                job.epoch = crate::attention::next(job.epoch)?;
                job.attempts = 0;
                job.retry_at_ms = 0;
                job.progress.reason = None;
                this.save(&key, &job).await
            })
            .await
    }
    async fn pass(&self) -> Result<SemanticPass, BoxError> {
        let (session, config, _) = self.context().await?;
        let limits = &self.bindings.config.limits;
        let deadline = Instant::now() + Duration::from_millis(limits.pass_ms);
        let ck = self.key(&config, "cursor")?;
        let mut cursor = self
            .directory
            .read::<Cursor>(&ck)
            .await?
            .map(|v| v.value)
            .unwrap_or_default();
        let mut pass = SemanticPass::default();
        if let Some(watch) = cursor.pending.clone() {
            self.process(&session, &config, &watch, &mut pass, deadline)
                .await?;
            cursor.after = watch;
            cursor.pending = None;
            self.save(&ck, &cursor).await?;
        }
        let request = crate::kip::request_with(
            format!(
                r#"FIND(?w.id, ?w._system.version, ?w.facets["WatchState"].arm_generation)
WHERE {{?w CONCEPT {{type:"Watch"}} FILTER(?w.attributes.status == "armed") FILTER(?w.id > :after)
FILTER(IS_NOT_NULL(?w.facets["WatchState"].arm_generation))
FILTER(IS_NOT_NULL(?w.attributes.condition.text) || (IS_NULL(?w.attributes.condition.element) && IS_NULL(?w.attributes.condition.slot) && IS_NULL(?w.attributes.condition.type)))}}
ORDER BY ?w.id LIMIT {}"#,
                limits.watches_per_pass
            ),
            crate::kip::param("after", cursor.after.as_str()),
        );
        let response = crate::kip::execute_readonly_request(&session, &request).await;
        let rows = crate::kip::ok_result(&response)
            .and_then(Json::as_array)
            .ok_or("semantic Watch discovery unavailable")?;
        let mut complete = true;
        for row in rows {
            if pass.scanned >= limits.watches_per_pass || Instant::now() >= deadline {
                complete = false;
                break;
            }
            let watch = row[0].as_str().ok_or("invalid Watch ID")?;
            let version = row[1].as_u64().ok_or("invalid Watch version")?;
            let generation = row[2].as_u64().ok_or("invalid Watch generation")?;
            let key = self.key(&config, &format!("jobs/{watch}"))?;
            let old = self.directory.read::<Job>(&key).await?;
            if old
                .as_ref()
                .is_none_or(|v| v.value.version != version || v.value.generation != generation)
            {
                if old.as_ref().is_some_and(|v| v.value.evaluation.is_some()) {
                    return Err(
                        "pending semantic commit must be recovered before replacing its input"
                            .into(),
                    );
                }
                self.save(
                    &key,
                    &Job {
                        watch: watch.into(),
                        version,
                        generation,
                        epoch: 0,
                        attempts: 0,
                        ticket: None,
                        evaluation: None,
                        accepted: false,
                        progress: Default::default(),
                        retry_at_ms: 0,
                    },
                )
                .await?;
            }
            cursor.pending = Some(watch.into());
            self.save(&ck, &cursor).await?;
            self.process(&session, &config, watch, &mut pass, deadline)
                .await?;
            cursor.after = watch.into();
            cursor.pending = None;
            self.save(&ck, &cursor).await?;
        }
        if complete && rows.len() < limits.watches_per_pass {
            cursor.after.clear();
        }
        cursor.last = pass.clone();
        self.save(&ck, &cursor).await?;
        Ok(pass)
    }
    async fn process(
        &self,
        session: &Session,
        config: &AttentionConfig,
        watch: &str,
        pass: &mut SemanticPass,
        deadline: Instant,
    ) -> Result<(), BoxError> {
        pass.scanned += 1;
        let key = self.key(config, &format!("jobs/{watch}"))?;
        let mut job = self
            .directory
            .read::<Job>(&key)
            .await?
            .ok_or("semantic pending job missing")?
            .value;
        let outcome = self
            .evaluate(session, config, &key, &mut job, pass, deadline)
            .await;
        if let Err(error) = outcome {
            // Only definite native refusals may release the pending slot.
            // Ambiguous storage failures preserve the exact Artifact/key for replay.
            if error.downcast_ref::<anda_kip::KipError>().is_some_and(|e| {
                matches!(
                    e.code,
                    anda_kip::KipErrorCode::VersionConflict
                        | anda_kip::KipErrorCode::NotAuthorized
                        | anda_kip::KipErrorCode::NotFoundOrNotVisible
                        | anda_kip::KipErrorCode::UnsupportedCapability
                        | anda_kip::KipErrorCode::ResourceExhausted
                )
            }) {
                job.evaluation = None;
                job.attempts = self.bindings.config.limits.attempts_per_page;
                job.progress.reason = Some("semantic_native_basis_or_material_unavailable".into());
                self.save(&key, &job).await?;
            } else {
                return Err(error);
            }
        }
        if job.accepted {
            pass.advanced += 1;
            pass.fired += usize::from(job.progress.status.as_deref() == Some("fired"));
            pass.expired += usize::from(job.progress.status.as_deref() == Some("expired"));
        } else {
            pass.deferred += 1;
            pass.reason = job.progress.reason.clone();
        }
        Ok(())
    }
    async fn evaluate(
        &self,
        session: &Session,
        config: &AttentionConfig,
        key: &str,
        job: &mut Job,
        pass: &mut SemanticPass,
        deadline: Instant,
    ) -> Result<(), BoxError> {
        let limits = &self.bindings.config.limits;
        if job.accepted {
            return Ok(());
        }
        if job.evaluation.is_none()
            && (job.retry_at_ms > anda_engine::unix_ms()
                || job.attempts >= limits.attempts_per_page)
        {
            job.progress
                .reason
                .get_or_insert_with(|| "semantic_attempt_budget_exhausted".into());
            return Ok(());
        }
        if job.ticket.is_none() {
            let prep = content_digest(
                &json!({"format":FORMAT,"scope":config.scope,"pin":self.pin,"watch":job.watch,"version":job.version,"generation":job.generation}),
            )?;
            let value = session
                .prepare_watch_page(
                    DEFAULT_SPACE,
                    &job.watch,
                    job.version,
                    job.generation,
                    limits.changes_per_page,
                    &prep,
                )
                .await?;
            job.ticket = Some(
                value["ticket_ref"]
                    .as_str()
                    .ok_or("native prepared page missing")?
                    .into(),
            );
            job.progress.ticket_ref = job.ticket.clone();
            self.save(key, job).await?;
        }
        let ticket = job.ticket.clone().unwrap();
        let page = session
            .read_prepared_watch_page(DEFAULT_SPACE, &ticket)
            .await?;
        if page.evaluator.as_ref() != Some(&self.pin)
            || page.watch_ref != job.watch
            || page.expected_version != job.version
            || page.arm_generation != job.generation
        {
            return Err("semantic prepared page does not match host intent".into());
        }
        let evaluation: WatchEvaluation = if let Some(pin) = &job.evaluation {
            serde_json::from_value(session.read_artifact(DEFAULT_SPACE, pin).await?)?
        } else {
            let response = crate::kip::execute_readonly_request(session, &crate::kip::request_with(
                r#"FIND(?w._system.version, ?w.attributes.status, ?w.facets["WatchState"].arm_generation) WHERE {?w CONCEPT {id: :watch}} LIMIT 1"#,
                crate::kip::param("watch",job.watch.as_str()))).await;
            let row = crate::kip::ok_result(&response).and_then(|v| v.get(0));
            if row.is_none_or(|r| r[0] != job.version || r[1] != "armed" || r[2] != job.generation)
            {
                return Err(anda_kip::KipError::new(
                    anda_kip::KipErrorCode::VersionConflict,
                    "semantic Watch changed before model dispatch",
                )
                .into());
            }
            if page.candidates.len() > limits.candidates_per_page {
                job.progress.reason = Some("semantic_page_budget_exceeded".into());
                job.attempts = limits.attempts_per_page;
                self.save(key, job).await?;
                return Ok(());
            }
            let request = request(&self.bindings.config, &page)?;
            let serialized = serde_json::to_string(&request)?;
            let tokens = if serialized.len() <= 1_048_576 {
                crate::recall_budget::count(&serialized)?
            } else {
                usize::MAX
            };
            if !page.candidates.is_empty()
                && (tokens > limits.input_tokens
                    || tokens > limits.pass_input_tokens.saturating_sub(pass.input_tokens)
                    || Instant::now() >= deadline)
            {
                job.progress.reason = Some("semantic_input_or_pass_budget_exhausted".into());
                self.save(key, job).await?;
                return Ok(());
            }
            job.attempts += 1;
            job.progress.reason = None;
            self.save(key, job).await?; // Reserve spend before the external callback.
            let judgments = if page.candidates.is_empty() {
                vec![]
            } else {
                pass.calls += 1;
                pass.input_tokens += tokens;
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(limits.callback_ms));
                let response = tokio::time::timeout(
                    remaining,
                    std::panic::AssertUnwindSafe(self.bindings.evaluator.evaluate(request))
                        .catch_unwind(),
                )
                .await;
                let result = match response {
                    Ok(Ok(Ok(raw))) => judgments(&raw, &page, limits.output_tokens)
                        .map_err(|_| "semantic_invalid_or_truncated_receipt"),
                    Ok(Ok(Err(_))) => Err("semantic_provider_unavailable"),
                    Ok(Err(_)) => Err("semantic_evaluator_panicked"),
                    Err(_) => Err("semantic_evaluator_timeout"),
                };
                match result {
                    Ok(rows) => rows,
                    Err(reason) => {
                        job.progress.reason = Some(reason.into());
                        unknown(&page, reason)
                    }
                }
            };
            let evaluation = WatchEvaluation {
                evaluation_key: content_digest(
                    &json!({"ticket":ticket,"pin":self.pin,"epoch":job.epoch,"attempt":job.attempts}),
                )?,
                evaluator: Some(self.pin()),
                judgments,
            };
            // Sources bind BOTH the prepared page and the recoverable response to
            // current authorization and purge. No plaintext in the host journal.
            let sources = std::iter::once(job.watch.clone())
                .chain(
                    page.candidates
                        .iter()
                        .filter_map(|c| c.change["id"].as_str().map(String::from)),
                )
                .collect();
            job.evaluation = Some(
                session
                    .put_artifact(DEFAULT_SPACE, json!(evaluation), sources)
                    .await?,
            );
            self.save(key, job).await?;
            evaluation
        };
        // Refresh host identity/configuration as well as the native rechecks.
        let (current, current_config, _) = self.context().await?;
        if current_config != *config {
            return Err("semantic configuration changed during evaluation".into());
        }
        let result = current
            .commit_watch_page(DEFAULT_SPACE, &ticket, evaluation)
            .await?;
        job.progress.status = result["status"].as_str().map(String::from);
        job.progress.evaluation_ref = result["evaluation_ref"].as_str().map(String::from);
        job.accepted = result["status"] != "deferred";
        if job.accepted {
            job.progress.reason = None;
        } else {
            job.progress
                .reason
                .get_or_insert_with(|| "semantic_unknown".into());
        }
        job.evaluation = None;
        job.retry_at_ms = anda_engine::unix_ms() + limits.retry_ms;
        self.save(key, job).await
    }
}
