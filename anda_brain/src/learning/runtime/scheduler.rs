//! One bounded, owned pass per Space. Watch discovery never enrolls a cohort;
//! only the installed factory's frozen manifests can enter this runtime.
use super::*;
use crate::learning::{
    EnrollmentOrigin, LearningBindings, LearningEnrollment, LearningPass, LearningRuntimeStatus,
};

impl LearningRuntime {
    pub async fn install_bindings(
        self: &Arc<Self>,
        auth: AuthContext,
        bindings: LearningBindings,
    ) -> Result<(), BoxError> {
        Self::require_host(&auth)?;
        bindings.validate()?;
        let cancel = self.cancel.child_token();
        let ready = self
            .bounded(bindings.callback_timeout_ms, async {
                bindings.executor.preflight(cancel.clone()).await?;
                let observer = bindings.observer.authenticate(cancel.clone()).await?;
                if observer.principal_id != bindings.registration.observer.principal_id
                    || observer.auth_strength == "none"
                    || observer.auth_method.is_empty()
                    || !observer.delegation_chain.is_empty()
                {
                    return Err(
                        "automatic learning observer authentication does not match registration"
                            .into(),
                    );
                }
                self.nexus
                    .session(observer.clone())
                    .effective_authority(DEFAULT_SPACE)
                    .await?
                    .authorize(
                        anda_cognitive_nexus::governance::Permission::RecordOutcome,
                        &anda_cognitive_nexus::governance::ResourceContext::default(),
                        &observer,
                    )
                    .into_result()?;
                Ok(())
            })
            .await;
        cancel.cancel();
        ready?;
        self.configure(auth.clone(), bindings.registration.clone())
            .await?;
        self.configure_storage(auth, bindings.storage.clone())
            .await?;
        let mut installed = self.bindings.write();
        if installed.is_some() {
            return Err("learning callbacks are already installed for this owner".into());
        }
        *installed = Some(Arc::new(bindings));
        Ok(())
    }
    pub async fn runtime_status(
        &self,
        automatic: bool,
        include_inventory: bool,
    ) -> Result<LearningRuntimeStatus, BoxError> {
        let bindings = self.bindings.read().clone();
        let mut status = LearningRuntimeStatus {
            compiled: true,
            registered: self.is_configured(),
            bindings_ready: bindings.is_some(),
            automatic_allowed: automatic,
            running: self.scheduler_running.load(Ordering::SeqCst),
            ..Default::default()
        };
        let mut calibration_reviewed = false;
        let mut identity_valid = false;
        if let Some(b) = bindings {
            status.automation = b.automation.clone();
            identity_valid = b.validate().is_ok();
            calibration_reviewed = b.calibration["approved_for_automatic_trials"] == true;
        }
        if status.registered {
            status.registration_enabled = self.registration(false).await?.enabled;
            if include_inventory {
                status.capacity = Some(self.capacity().await?);
                status.last_pass = self
                    .journal
                    .read::<LearningPass>("scheduler/last-pass")
                    .await?
                    .map(|r| r.value);
            }
        } else {
            status
                .blocked_reasons
                .push("learning_not_registered".into());
        }
        if !status.bindings_ready {
            status
                .blocked_reasons
                .push("learning_bindings_not_installed".into());
        }
        if !automatic {
            status
                .blocked_reasons
                .push("automatic_host_work_disabled".into());
        }
        if status.registered && !status.registration_enabled {
            status
                .blocked_reasons
                .push("learning_registration_disabled".into());
        }
        if !status.automation.trials {
            status
                .blocked_reasons
                .push("automatic_trials_disabled".into());
        }
        if !status.automation.reviews {
            status
                .blocked_reasons
                .push("automatic_reviews_disabled".into());
        }
        let (state, next_step) = if !status.bindings_ready {
            (
                "services_missing",
                "install_workflow_http_v1_with_separate_executor_observer_source",
            )
        } else if !identity_valid {
            (
                "identity_mismatch",
                "revalidate_frozen_contract_and_service_identities",
            )
        } else if !calibration_reviewed {
            (
                "calibration_missing",
                "review_task_family_specific_train_validation_evidence",
            )
        } else if !status.registration_enabled
            || !automatic
            || !status.automation.trials
            || !status.automation.reviews
        {
            (
                "awaiting_approval",
                "review_deployment_and_enable_each_automation_explicitly",
            )
        } else {
            (
                "ready",
                "native_authority_and_service_readiness_rechecked_per_work_item",
            )
        };
        status.product_readiness = json!({"state":state,"next_step":next_step,"template":"tool_workflow.precondition.v1","per_item_revalidation":true});
        Ok(status)
    }

    /// Start without blocking the attention scan on business I/O. Owned task
    /// tracking makes eviction/shutdown wait for checkpoints and cancellation.
    pub(crate) fn kick(
        self: &Arc<Self>,
        consequences: Option<Arc<crate::consequence::ConsequenceRuntime>>,
        automatic: bool,
    ) {
        if !automatic
            || !self.is_configured()
            || self.bindings.read().is_none()
            || self.ensure_open().is_err()
            || self.scheduler_running.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let this = self.clone();
        self.tasks.spawn(async move {
            let result = std::panic::AssertUnwindSafe(this.automatic_pass(consequences)).catch_unwind().await;
            match result {
                Ok(Ok(_)) => {},
                Ok(Err(e)) => log::warn!(target: "brain", "learning pass blocked: {e}"),
                Err(_) => log::warn!(target: "brain", "learning pass panicked; retained intents require recovery"),
            }
            this.scheduler_running.store(false, Ordering::SeqCst);
        });
    }
    async fn bounded<T>(
        &self,
        ms: u64,
        operation: impl std::future::Future<Output = Result<T, BoxError>>,
    ) -> Result<T, BoxError> {
        tokio::select! {
            _ = self.cancel.cancelled() => Err("learning host cancelled".into()),
            r = tokio::time::timeout(std::time::Duration::from_millis(ms), operation) => r.map_err(|_| "learning callback timed out")?,
        }
    }
    pub(super) async fn bind_enrollment_origin(
        &self,
        reg: &Registration,
        id: &str,
        prepared: &mut Option<Job>,
        origin: Option<EnrollmentOrigin>,
    ) -> Result<(), BoxError> {
        let Some(origin) = origin else {
            if let Some(job) = prepared
                && job.origin.is_none()
                && self
                    .journal
                    .read::<Job>(&format!("enrollments/{}", &Self::key(id)?[5..]))
                    .await?
                    .is_none()
            {
                job.origin = Some(EnrollmentOrigin {
                    source_id: "direct-rust-host".into(),
                    source_digest: content_digest(&json!("anda-brain:direct-enrollment-v1"))?,
                    event_key: id.into(),
                    trigger_ref: None,
                    gate_wake_ref: None,
                });
            }
            return Ok(());
        };
        origin.validate()?;
        if let Some(trigger) = &origin.trigger_ref {
            self.read_one(format!(
                "FIND(?r) WHERE {{?r ACTIVITY {{id:{}}}}} LIMIT 1",
                serde_json::to_string(trigger)?
            ))
            .await?;
        }
        if let Some(wake_ref) = &origin.gate_wake_ref {
            let wake = self
                .nexus
                .system_session()
                .read_wake(DEFAULT_SPACE, wake_ref)
                .await?;
            if !matches!(
                wake.state,
                anda_cognitive_nexus::attention::WakeState::Completed { .. }
            ) || origin.trigger_ref.as_deref() != Some(wake.fire_activity_ref.as_str())
            {
                return Err(
                    "learning origin requires the actual completed gate and firing activity".into(),
                );
            }
        }
        if let Some(job) = prepared {
            if job.origin.as_ref().is_some_and(|old| old != &origin) {
                return Err("enrollment origin conflicts with reserved identity".into());
            }
            job.origin = Some(origin);
        } else if self.load_job(reg, id).await?.value.origin.as_ref() != Some(&origin) {
            return Err("enrollment origin conflicts with frozen job".into());
        }
        Ok(())
    }
    async fn enroll_registered(
        self: &Arc<Self>,
        b: &LearningBindings,
        enrollment: LearningEnrollment,
        review: Option<&ReviewStatus>,
    ) -> Result<JobReport, BoxError> {
        let source = b.plans.source();
        if (
            enrollment.origin.source_id.clone(),
            enrollment.origin.source_digest.clone(),
        ) != source
        {
            return Err("unregistered learning task source".into());
        }
        b.registration
            .validate_executor(&enrollment.plan, &b.executor.identity())?;
        if let Some(review) = review {
            return self
                .enroll_review_origin(
                    review.job_id.clone(),
                    enrollment.job_id,
                    enrollment.plan,
                    enrollment.basis_proposition,
                    Some(enrollment.origin),
                )
                .await;
        }
        if enrollment.plan.execution.review_of.is_some() {
            return Err("initial source cannot bypass the review schedule".into());
        }
        let this = self.clone();
        self.owned(move |_| {
            Box::pin(async move {
                let _g = this.gate.lock().await;
                let reg = this.registration(true).await?;
                this.recover_catalog().await?;
                let mut prepared = this
                    .prepare_enrollment(
                        &reg,
                        &enrollment.job_id,
                        enrollment.plan,
                        enrollment.basis_proposition,
                    )
                    .await?;
                this.bind_enrollment_origin(
                    &reg,
                    &enrollment.job_id,
                    &mut prepared,
                    Some(enrollment.origin),
                )
                .await?;
                this.commit_enrollment(&reg, &enrollment.job_id, prepared)
                    .await
            })
        })
        .await
    }
    async fn poll_observer(
        self: &Arc<Self>,
        b: &LearningBindings,
        id: &str,
        consequences: &Arc<crate::consequence::ConsequenceRuntime>,
    ) -> Result<bool, BoxError> {
        let reg = self.registration(false).await?;
        let ticket = {
            let _g = self.gate.lock().await;
            let job = self.load_job(&reg, id).await?.value;
            let Some(attempt) = job.attempts.values().find(|a| {
                a.outcome_ref.is_none()
                    && !matches!(
                        a.state,
                        DispatchState::Prepared | DispatchState::Authorizing
                    )
            }) else {
                return Ok(false);
            };
            self.hydrate_ticket(&job, attempt).await?
        };
        let token = self.cancel.child_token();
        let result = self
            .bounded(b.callback_timeout_ms, async {
                let auth = b.observer.authenticate(token.clone()).await?;
                let Some(input) = b.observer.observe(&ticket, token.clone()).await? else {
                    return Ok(None);
                };
                Ok(Some((auth, input)))
            })
            .await;
        token.cancel();
        let Some((auth, observation)) = result? else {
            return Ok(false);
        };
        let input = observation.input;
        if serde_json::to_vec(&observation.replay)?.len() > 16_384
            || !matches!(&input.observation, crate::consequence::Observation::Learning { measurements } if measurements["journal_digest"] == content_digest(&observation.replay)?)
        {
            return Err("independent workflow replay is missing, mismatched or over budget".into());
        }
        let replay_key = format!(
            "observations/{}",
            &content_digest(&json!([
                input.space_instance,
                input.event_key,
                auth.principal_id
            ]))?[7..]
        );
        {
            let _g = self.gate.lock().await;
            let mut job = self.load_job(&reg, id).await?;
            let a = job
                .value
                .attempts
                .values_mut()
                .find(|a| a.ticket.dispatch_id == ticket.dispatch_id)
                .ok_or("observation attempt disappeared")?;
            if a.replay_key.as_ref().is_some_and(|k| k != &replay_key) {
                return Err("attempt already has different independent replay material".into());
            }
            self.journal
                .create(&replay_key, &observation.replay)
                .await?;
            a.replay_key = Some(replay_key);
            self.journal.save(&Self::key(id)?, &job).await?;
        }
        // No timeout around the owned durable ingress. Only LearningRuntime
        // writes trial Outcomes, including native-ACK recovery.
        let receipt = consequences.submit(auth, input).await?;
        Ok(receipt.learning_eligible)
    }
    pub(super) async fn automatic_pass(
        self: &Arc<Self>,
        consequences: Option<Arc<crate::consequence::ConsequenceRuntime>>,
    ) -> Result<LearningPass, BoxError> {
        let _pass = self.scheduler_gate.lock().await;
        self.ensure_open()?;
        let b = self
            .bindings
            .read()
            .clone()
            .ok_or("learning bindings missing")?;
        let mut report = LearningPass {
            started_at_ms: anda_engine::unix_ms(),
            ..Default::default()
        };
        let reg = self.registration(false).await?;
        let token = self.cancel.child_token();
        let ready = if reg.enabled && b.automation.trials && consequences.is_some() {
            self.bounded(b.callback_timeout_ms, async {
                b.executor.preflight(token.clone()).await?;
                let auth = b.observer.authenticate(token.clone()).await?;
                if auth.principal_id != reg.config.observer.principal_id
                    || auth.auth_strength == "none"
                    || auth.auth_method.is_empty()
                    || !auth.delegation_chain.is_empty()
                {
                    return Err("independent observer authentication changed".into());
                }
                self.nexus
                    .session(auth.clone())
                    .effective_authority(DEFAULT_SPACE)
                    .await?
                    .authorize(
                        anda_cognitive_nexus::governance::Permission::RecordOutcome,
                        &anda_cognitive_nexus::governance::ResourceContext::default(),
                        &auth,
                    )
                    .into_result()?;
                Ok(())
            })
            .await
            .is_ok()
        } else {
            false
        };
        if reg.enabled && b.automation.trials && !ready {
            report
                .blocked
                .push("automatic_learning_readiness_unavailable".into());
        }
        if b.automation.safety {
            if let Some(consequences) = &consequences {
                match self
                    .bounded(
                        b.callback_timeout_ms,
                        b.observer.authenticate(token.clone()),
                    )
                    .await
                {
                    Ok(auth) => match consequences.consume_learning_safety(auth, 8).await {
                        Ok(n) => report.safety_resolved += n,
                        Err(_) => report
                            .blocked
                            .push("learning_safety_ingress_requires_recovery".into()),
                    },
                    Err(_) => report
                        .blocked
                        .push("learning_observer_authentication_unavailable".into()),
                }
            } else {
                report
                    .blocked
                    .push("learning_observation_ingress_not_installed".into());
            }
            for id in self.safety_jobs().await?.into_iter().take(1) {
                match self.settle(id).await {
                    Ok(r)
                        if r.safety
                            .as_ref()
                            .is_some_and(|s| s.evaluation_ref.is_some()) =>
                    {
                        report.safety_resolved += 1
                    }
                    _ => report
                        .blocked
                        .push("learning_safety_requires_fresh_observer_or_native_recovery".into()),
                }
            }
        }
        let (review_cursor, drive_cursor) = self.scheduler_cursors().await?;
        let jobs = self.jobs().await?;
        if let Some(row) = jobs.get(drive_cursor % jobs.len().max(1)) {
            if ready {
                if let Some(consequences) = &consequences {
                    // Native drive performs one dispatch/reconcile at most.
                    if !matches!(row.stage, JobStage::Settled | JobStage::Expired)
                        && self.clock.now_ms() < time_ms(&row.cutoff)?
                    {
                        match self.drive(row.job_id.clone(), b.executor.clone()).await {
                            Ok(_) => report.driven += 1,
                            Err(_) => report.blocked.push(
                                "learning_drive_requires_recovery_or_current_authority".into(),
                            ),
                        }
                    }
                    match self.poll_observer(&b, &row.job_id, consequences).await {
                        Ok(true) => report.observed += 1,
                        Ok(false) => {}
                        Err(_) => report
                            .blocked
                            .push("learning_observation_pending_or_rejected".into()),
                    }
                    if row.stage != JobStage::Settled
                        && self.clock.now_ms() >= time_ms(&row.cutoff)?
                    {
                        match self.settle(row.job_id.clone()).await {
                            Ok(_) => report.settled += 1,
                            Err(_) => report
                                .blocked
                                .push("learning_fixed_cutoff_settlement_blocked".into()),
                        }
                    }
                } else {
                    report
                        .blocked
                        .push("learning_observation_ingress_not_installed".into());
                }
            }
            if b.automation.archive
                && matches!(
                    self.report(&row.job_id).await?.stage,
                    JobStage::Settled | JobStage::Expired
                )
            {
                match self.archive(row.job_id.clone()).await {
                    Ok(_) => report.archived += 1,
                    Err(_) => report
                        .blocked
                        .push("learning_archive_has_unresolved_dispatch_or_receipt".into()),
                }
            }
        }
        let page = self.reviews_page(review_cursor, 8).await?;
        report.reviews_checked = page.items.len();
        if ready
            && b.automation.reviews
            && let Some(review) = page.items.iter().find(|r| r.due)
        {
            match self
                .bounded(
                    b.callback_timeout_ms,
                    b.plans
                        .next(&reg.instance, None, Some(review), token.clone()),
                )
                .await
            {
                Ok(Some(enrollment)) => {
                    match self.enroll_registered(&b, enrollment, Some(review)).await {
                        Ok(_) => report.reviews_enrolled += 1,
                        Err(_) => report
                            .blocked
                            .push("review_enrollment_blocked_obligation_retained".into()),
                    }
                }
                Ok(None) => report
                    .blocked
                    .push("review_due_without_fresh_authorized_cohort".into()),
                Err(_) => report
                    .blocked
                    .push("review_plan_factory_unavailable".into()),
            }
        }
        if ready
            && report.reviews_enrolled == 0
            && self.check_capacity(reg.config.maximum_jobs).await.is_ok()
            && !self
                .jobs()
                .await?
                .iter()
                .any(|r| !matches!(r.stage, JobStage::Settled | JobStage::Expired))
        {
            let after = self
                .journal
                .read::<String>("scheduler/source-cursor")
                .await?;
            match self
                .bounded(
                    b.callback_timeout_ms,
                    b.plans.next(
                        &reg.instance,
                        after.as_ref().map(|r| r.value.as_str()),
                        None,
                        token.clone(),
                    ),
                )
                .await
            {
                Ok(Some(enrollment)) => {
                    let event = enrollment.origin.event_key.clone();
                    match self.enroll_registered(&b, enrollment, None).await {
                        Ok(_) => {
                            if let Some(mut old) = after {
                                old.value = event;
                                self.journal.save("scheduler/source-cursor", &old).await?;
                            } else {
                                self.journal
                                    .create("scheduler/source-cursor", &event)
                                    .await?;
                            }
                            report.enrolled += 1;
                        }
                        Err(_) => report
                            .blocked
                            .push("registered_source_enrollment_blocked".into()),
                    }
                }
                Ok(None) => {}
                Err(_) => report
                    .blocked
                    .push("registered_plan_factory_unavailable".into()),
            }
        }
        token.cancel();
        self.advance_scheduler(
            if page.complete { 0 } else { page.next_after },
            (drive_cursor + 1) % jobs.len().max(1),
        )
        .await?;
        report.finished_at_ms = anda_engine::unix_ms();
        if !reg.enabled {
            report.blocked.push("learning_registration_disabled".into());
        }
        if !b.automation.trials {
            report.blocked.push("automatic_trials_disabled".into());
        }
        report.blocked.sort();
        report.blocked.dedup();
        if let Some(mut old) = self
            .journal
            .read::<LearningPass>("scheduler/last-pass")
            .await?
        {
            old.value = report.clone();
            self.journal.save("scheduler/last-pass", &old).await?;
        } else {
            self.journal.create("scheduler/last-pass", &report).await?;
        }
        Ok(report)
    }
}
