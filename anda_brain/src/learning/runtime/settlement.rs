//! Durable intent/checkpoint protocol around native atomic learning verdicts.
use super::*;
use crate::learning::{AdoptionBasis, native::settlement::*};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VerdictPurpose {
    Activation,
    Settlement,
    Safety,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PendingVerdict {
    purpose: VerdictPurpose,
    intent: PreparedVerdict,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetySubmission {
    pub space_instance: String,
    pub job_id: String,
    pub signal_key: String,
    pub observed_at: String,
    pub evidence_digest: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PendingSafety {
    digest: String,
    input: NativeSafetySignalInput,
    evidence_ref: Option<String>,
    pub(super) evaluation_ref: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SafetyReport {
    pub evidence_ref: Option<String>,
    pub evaluation_ref: Option<String>,
    pub requires_observer_retry: bool,
}
impl PendingSafety {
    pub(super) fn report(&self) -> SafetyReport {
        SafetyReport {
            evidence_ref: self.evidence_ref.clone(),
            evaluation_ref: self.evaluation_ref.clone(),
            requires_observer_retry: self.evidence_ref.is_none(),
        }
    }
}

impl LearningRuntime {
    /// Route a later signal to the revision's current controller job. A prior
    /// immutable revocation can cover another signal only while that exact
    /// native verdict still governs the current revision.
    pub(crate) async fn safety_target(
        &self,
        source_id: &str,
        auth: &AuthContext,
    ) -> Result<(String, Option<String>), BoxError> {
        let reg = self.registration(false).await?;
        if auth.principal_id != reg.config.observer.principal_id
            || auth.auth_method.is_empty()
            || auth.auth_strength == "none"
            || !auth.delegation_chain.is_empty()
        {
            return Err("fresh registered safety observer required".into());
        }
        let source = self.load_job(&reg, source_id).await?.value;
        let session = self.nexus.session(auth.clone());
        let revision =
            crate::runtime_api::full_read(&session, &source.plan.candidate_revision).await?;
        let skill =
            crate::runtime_api::full_read(&session, applicability::edge(&revision, "revision_of")?)
                .await?;
        let Some(evaluation) =
            crate::learning::native::settlement::pointer(&skill, "current_evaluation")
        else {
            return Ok((source_id.into(), None));
        };
        // `current_evaluation` is cleared when the selected revision changes.
        if applicability::edge(&skill, "current_revision")? != source.plan.candidate_revision {
            return Err("safety revision is not the currently graded revision".into());
        }
        if skill["attributes"]["status"] == "revoked" {
            let record = crate::runtime_api::full_read(&session, evaluation).await?;
            let e = &record["facets"][profile!("EvaluationRecord")];
            if e["to_status"] == "revoked"
                && e["revision_refs"] == json!([source.plan.candidate_revision])
            {
                return Ok((source_id.into(), Some(evaluation.into())));
            }
        }
        let mut target = self.indexed_job("evaluations", evaluation).await?;
        for report in self.jobs().await? {
            if report.evaluation_ref.as_deref() == Some(evaluation)
                || report.activation_ref.as_deref() == Some(evaluation)
            {
                target = Some(report.job_id);
                break;
            }
        }
        let target = target.unwrap_or_else(|| source_id.into());
        if self
            .load_job(&reg, &target)
            .await?
            .value
            .plan
            .candidate_revision
            != source.plan.candidate_revision
        {
            return Err("safety target changed revision".into());
        }
        Ok((target, None))
    }
    pub(super) fn verdict_input(
        reg: &Registration,
        job: &Job,
        label: &str,
    ) -> Result<NativeVerdictInput, BoxError> {
        Ok(NativeVerdictInput {
            plan: job.plan.clone(),
            frozen: job.frozen.clone().ok_or("frozen policy missing")?,
            trial_ref: job.trial_ref.clone().ok_or("trial missing")?,
            idempotency_key: Self::native_key(
                &reg.instance,
                &job.id,
                &format!("{label}:{}", job.verdict_generation),
            )?,
        })
    }

    pub(super) async fn activate_trial(
        &self,
        reg: &Registration,
        id: &str,
    ) -> Result<(), BoxError> {
        for _ in 0..3 {
            let mut state = self.load_job(reg, id).await?;
            if state.value.activation_complete {
                return Ok(());
            }
            if state.value.verdict_pending.is_none() {
                let native = self.native_for(&reg.config).await?;
                let intent = native
                    .prepare_activation(
                        &Self::verdict_input(reg, &state.value, "activation")?,
                        &anda_cognitive_nexus::time::now(),
                    )
                    .await?;
                let Some(intent) = intent else {
                    state.value.activation_complete = true;
                    self.journal.save(&Self::key(id)?, &state).await?;
                    return Ok(());
                };
                state.value.verdict_pending = Some(PendingVerdict {
                    purpose: VerdictPurpose::Activation,
                    intent,
                });
                self.journal.save(&Self::key(id)?, &state).await?;
            }
            if self.resume_verdict(reg, id).await? {
                return Ok(());
            }
        }
        Err("activation changed concurrently; reread on the next host step".into())
    }

    /// Close the predeclared observation window and recompute the complete
    /// native ledger. A final receipt is replayed, never recalculated using a
    /// later favorable window. No caller supplies a score or sample selection.
    pub async fn settle(self: &Arc<Self>, job_id: String) -> Result<JobReport, BoxError> {
        self.owned(move |this| Box::pin(async move {
            let _g = this.gate.lock().await;
            this.ensure_open()?;
            let reg = this.registration(false).await?;
            this.nexus.recover().await?;
            for _ in 0..3 {
                let mut state = this.load_job(&reg, &job_id).await?;
                if state.value.verdict_pending.is_some() {
                    this.resume_verdict(&reg, &job_id).await?;
                    continue;
                }
                if let Some(safety) = &state.value.safety
                    && safety.evaluation_ref.is_none()
                {
                    let evidence = safety.evidence_ref.as_deref().ok_or("pending safety observation needs freshly authenticated observer recovery")?;
                    let intent = this.native_for(&reg.config).await?.prepare_safety_revocation(evidence,
                        &Self::native_key(&reg.instance, &job_id, &format!("safety-verdict:{}", state.value.verdict_generation))?
                    ).await?;
                    state.value.verdict_pending = Some(PendingVerdict { purpose: VerdictPurpose::Safety, intent });
                    this.journal.save(&Self::key(&job_id)?, &state).await?;
                    continue;
                }
                if state.value.stage == JobStage::Settled {
                    if let Some(stamp) = &state.value.archive { this.index_history(&state.value, stamp.slot).await?; }
                    if state.value.safety.as_ref().is_some_and(|s| s.evaluation_ref.is_some()) {
                        this.track_safety(&job_id, &state.value.plan.candidate_revision, false).await?;
                    }
                    return Ok(state.value.report());
                }
                state.value.plan.execution.validate_settlement(
                    &state.value.plan.execution.cutoff, &crate::kip::timestamp(this.clock.now_ms())
                )?;
                if state.value.pending.as_ref().is_some_and(|p| !matches!(p.intent.operation, NativeOperation::Outcome(_))) {
                    this.resume_native(&reg, &job_id, None).await?;
                    continue;
                }
                // No complete baseline means no legal Trial can be created.
                // Keep the failed enrollment auditable without inventing proof.
                if state.value.trial_ref.is_none() || !state.value.activation_complete {
                    state.value.stage = JobStage::Expired;
                    this.journal.save(&Self::key(&job_id)?, &state).await?;
                    return Ok(state.value.report());
                }
                if !reg.enabled { return Err("learning is disabled; no new comparison verdict is authorized".into()); }
                let native = this.native_for(&reg.config).await?;
                let intent = native.prepare_settlement(
                    &Self::verdict_input(&reg, &state.value, "settlement")?,
                    &crate::kip::timestamp(this.clock.now_ms()),
                ).await?;
                state.value.verdict_pending = Some(PendingVerdict { purpose: VerdictPurpose::Settlement, intent });
                this.journal.save(&Self::key(&job_id)?, &state).await?;
                this.resume_verdict(&reg, &job_id).await?;
            }
            let state = this.load_job(&reg, &job_id).await?;
            if state.value.stage == JobStage::Settled { Ok(state.value.report()) }
            else { Err("settlement changed concurrently; reread on the next host step".into()) }
        })).await
    }

    /// `false` means a definite native CAS conflict was recovered as absent;
    /// the next bounded iteration must reread all material under a new key.
    pub(super) async fn resume_verdict(
        &self,
        reg: &Registration,
        id: &str,
    ) -> Result<bool, BoxError> {
        let mut state = self.load_job(reg, id).await?;
        let Some(pending) = state.value.verdict_pending.clone() else {
            return Ok(true);
        };
        let native = self.native_for(&reg.config).await?;
        let receipt = if let Some(receipt) = native.recover_verdict(&pending.intent).await? {
            receipt
        } else {
            if state.value.safety.is_some() && !matches!(pending.purpose, VerdictPurpose::Safety) {
                state.value.verdict_pending = None;
                state.value.verdict_generation = state
                    .value
                    .verdict_generation
                    .checked_add(1)
                    .ok_or("verdict generation exhausted")?;
                self.journal.save(&Self::key(id)?, &state).await?;
                return Ok(false);
            }
            if !reg.enabled && !matches!(pending.purpose, VerdictPurpose::Safety) {
                return Err("learning disabled; pending verdict has not committed".into());
            }
            let now = if matches!(
                pending.purpose,
                VerdictPurpose::Activation | VerdictPurpose::Safety
            ) {
                anda_cognitive_nexus::time::now()
            } else {
                crate::kip::timestamp(self.clock.now_ms())
            };
            #[cfg(feature = "experiments")]
            let committed = if self.clock.is_manual() {
                native
                    .execute_verdict_simulated(&pending.intent, &now)
                    .await
            } else {
                native.execute_verdict(&pending.intent, &now).await
            };
            #[cfg(not(feature = "experiments"))]
            let committed = native.execute_verdict(&pending.intent, &now).await;
            match committed {
                Ok(receipt) => receipt,
                Err(error) if error.code == anda_kip::KipErrorCode::VersionConflict => {
                    if let Some(receipt) = native.recover_verdict(&pending.intent).await? {
                        receipt
                    } else {
                        state.value.verdict_pending = None;
                        state.value.verdict_generation = state
                            .value
                            .verdict_generation
                            .checked_add(1)
                            .ok_or("verdict generation exhausted")?;
                        self.journal.save(&Self::key(id)?, &state).await?;
                        return Ok(false);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        };
        // The native verdict and plan already contain the schedule's source
        // facts. A lost host checkpoint is reconstructed with the same receipt.
        match pending.purpose {
            VerdictPurpose::Activation => {
                state.value.activation_ref = Some(receipt.evaluation_ref);
                state.value.activation_complete = true;
            }
            VerdictPurpose::Settlement | VerdictPurpose::Safety => {
                if matches!(pending.purpose, VerdictPurpose::Safety) {
                    state
                        .value
                        .safety
                        .as_mut()
                        .ok_or("safety journal missing")?
                        .evaluation_ref = Some(receipt.evaluation_ref.clone());
                }
                state.value.evaluation_ref = Some(receipt.evaluation_ref.clone());
                state.value.stage = JobStage::Settled;
                if receipt.to_status == "adopted" {
                    let acquisition = if pending.intent.from_status == "trialed" {
                        AdoptionBasis {
                            revision_ref: state.value.plan.candidate_revision.clone(),
                            trial_ref: state
                                .value
                                .trial_ref
                                .clone()
                                .ok_or("adoption trial missing")?,
                            evaluation_ref: receipt.evaluation_ref.clone(),
                        }
                    } else {
                        state
                            .value
                            .plan
                            .execution
                            .review_of
                            .clone()
                            .ok_or("monitoring acquisition missing")?
                    };
                    state.value.review = Some(ReviewSchedule {
                        due_at: state.value.plan.execution.review_due_at.clone(),
                        acquisition,
                        evaluation_ref: receipt.evaluation_ref,
                        next_job_id: None,
                        suspended: None,
                    });
                } else if let Some(acquisition) = state
                    .value
                    .plan
                    .execution
                    .review_of
                    .clone()
                    .or_else(|| state.value.review.as_ref().map(|r| r.acquisition.clone()))
                {
                    state.value.review = Some(ReviewSchedule {
                        due_at: state.value.plan.execution.review_due_at.clone(),
                        acquisition,
                        evaluation_ref: receipt.evaluation_ref,
                        next_job_id: None,
                        suspended: Some(if matches!(pending.purpose, VerdictPurpose::Safety) {
                            ReviewReason::SafetySignal
                        } else if receipt.comparison["status"] == "insufficient" {
                            ReviewReason::InsufficientEvidence
                        } else {
                            ReviewReason::NotImproved
                        }),
                    });
                }
            }
        }
        state.value.verdict_pending = None;
        self.journal.save(&Self::key(id)?, &state).await?;
        if let Some(stamp) = &state.value.archive {
            self.index_history(&state.value, stamp.slot).await?;
        }
        if state
            .value
            .safety
            .as_ref()
            .is_some_and(|s| s.evaluation_ref.is_some())
        {
            self.track_safety(id, &state.value.plan.candidate_revision, false)
                .await?;
        }
        Ok(true)
    }

    /// Receive a separately authenticated safety instrument signal and revoke
    /// immediately, without waiting for the comparison sample quota or cutoff.
    /// Neither the executor nor a model can invoke this through a Brain tool.
    pub async fn submit_safety_signal(
        self: &Arc<Self>,
        auth: AuthContext,
        submission: SafetySubmission,
    ) -> Result<JobReport, BoxError> {
        self.owned(move |this| Box::pin(async move {
            this.ensure_open()?;
            let reg = this.registration(false).await?;
            if auth.principal_id != reg.config.observer.principal_id
                || auth.auth_method.is_empty() || auth.auth_strength == "none"
                || !auth.delegation_chain.is_empty() || submission.space_instance != reg.instance
            { return Err("safety signal requires this Space's directly authenticated independent observer".into()); }
            if submission.signal_key.is_empty() || submission.signal_key.len() > 256
                || submission.reason.trim().is_empty() || submission.reason.len() > 2048
                || !crate::learning::is_digest(&submission.evidence_digest)
                || time_ms(&submission.observed_at)? > anda_engine::unix_ms()
            { return Err("invalid independent safety observation".into()); }
            let id = &submission.job_id;
            let digest = content_digest(&json!(submission))?;
            let prior = this.load_job(&reg, id).await?.value;
            if prior.safety.as_ref().is_some_and(|s| s.digest != digest) {
                return Err("job already has a different safety signal".into());
            }
            if prior.safety.is_none() {
                let observer_session = this.nexus.session(auth.clone());
                observer_session.effective_authority(DEFAULT_SPACE).await?.authorize(
                    anda_cognitive_nexus::governance::Permission::RecordOutcome,
                    &anda_cognitive_nexus::governance::ResourceContext::default(), &auth,
                ).into_result()?;
            }
            let safety_revision = prior.plan.candidate_revision.clone();
            // Publish the barrier before waiting for `gate`. A concurrent drive
            // either sees it after registering its token or is cancelled by the
            // check immediately below.
            let needs_revocation = prior
                .safety
                .as_ref()
                .is_none_or(|s| s.evaluation_ref.is_none());
            if needs_revocation {
                this.safety_barriers
                    .write()
                    .insert(digest.clone(), safety_revision.clone());
            }
            if needs_revocation
                && let Some((revision, cancel)) = this.active_dispatch.read().as_ref()
                && revision == &safety_revision
            { cancel.cancel(); }
            let _g = this.gate.lock().await;
            let native = this.native_for(&reg.config).await?;
            this.nexus.recover().await?;
            let mut state = this.load_job(&reg, id).await?;
            if let Some(old) = &state.value.safety {
                if old.digest != digest {
                    this.safety_barriers.write().remove(&digest);
                    return Err("job already has a different safety signal; use its native evidence for audit".into());
                }
                if old.evaluation_ref.is_some() {
                    this.track_safety(id, &safety_revision, false).await?;
                    this.safety_barriers.write().remove(&digest);
                    return Ok(state.value.report());
                }
            } else {
                this.track_safety(id, &safety_revision, true).await?;
                let input = NativeSafetySignalInput {
                    revision_ref: state.value.plan.candidate_revision.clone(),
                    observation_key: format!("safety:{}", Self::native_key(&reg.instance, id, &submission.signal_key)?),
                    observed_at: submission.observed_at.clone(),
                    journal_digest: submission.evidence_digest.clone(),
                    reason: submission.reason.clone(),
                    idempotency_key: Self::native_key(&reg.instance, id, &format!("safety-signal:{}", submission.signal_key))?,
                };
                state.value.safety = Some(PendingSafety { digest: digest.clone(), input, evidence_ref: None, evaluation_ref: None });
                this.journal.save(&Self::key(id)?, &state).await?;
                state = this.load_job(&reg, id).await?;
            }
            let signal = state.value.safety.as_ref().unwrap();
            if signal.evidence_ref.is_none() {
                let receipt = if let Some(found) = native.recover_safety_signal(&signal.input, &auth).await? { found }
                    else { native.record_safety_signal(&signal.input, &auth).await? };
                state.value.safety.as_mut().unwrap().evidence_ref = Some(receipt.evidence_ref);
                if let Some(review) = &mut state.value.review { review.suspended = Some(ReviewReason::SafetySignal); }
                this.journal.save(&Self::key(id)?, &state).await?;
            }
            for _ in 0..3 {
                let mut current = this.load_job(&reg, id).await?;
                if current.value.safety.as_ref().unwrap().evaluation_ref.is_some() {
                    this.safety_barriers.write().remove(&digest);
                    return Ok(current.value.report());
                }
                if let Some(pending) = &current.value.verdict_pending {
                    if !matches!(pending.purpose, VerdictPurpose::Safety) {
                        // Never execute a pending adoption after a safety signal.
                        // Recover a durable verdict; otherwise discard only an
                        // explicitly absent intent, retaining its generation.
                        if native.recover_verdict(&pending.intent).await?.is_some() {
                            this.resume_verdict(&reg, id).await?;
                            continue;
                        }
                        current.value.verdict_pending = None;
                        current.value.verdict_generation = current.value.verdict_generation.checked_add(1).ok_or("verdict generation exhausted")?;
                        this.journal.save(&Self::key(id)?, &current).await?;
                        continue;
                    }
                } else {
                    let signal_ref = current.value.safety.as_ref().unwrap().evidence_ref.as_deref().ok_or("native safety evidence missing")?;
                    let intent = native.prepare_safety_revocation(signal_ref, &Self::native_key(&reg.instance, id, &format!("safety-verdict:{}", current.value.verdict_generation))?).await?;
                    current.value.verdict_pending = Some(PendingVerdict { purpose: VerdictPurpose::Safety, intent });
                    this.journal.save(&Self::key(id)?, &current).await?;
                }
                this.resume_verdict(&reg, id).await?;
            }
            let result = this.load_job(&reg, id).await?.value;
            if result.safety.as_ref().unwrap().evaluation_ref.is_some() {
                this.safety_barriers.write().remove(&digest);
                Ok(result.report())
            }
            else { Err("safety revocation changed concurrently; recommendation remains suspended".into()) }
        })).await
    }
}
