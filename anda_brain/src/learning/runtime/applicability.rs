//! Read-time applicability is separate from historical standing and dispatch.
use super::*;
use crate::learning::AdoptionBasis;

const PROFILE: &str = "kip://profiles/cognitive-memory@2.1.0/";

/// A trusted host's short-lived observation of the actual application context.
/// Never accept this object from Recall/model arguments. A new process must
/// rebind it: persisted calibration and historic grades do not describe today.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationContext {
    pub executor: ExecutorIdentity,
    pub revision_ref: String,
    pub preconditions_satisfied: bool,
    pub evidence_digest: String,
    pub checked_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReason {
    Deadline,
    EnvironmentChanged,
    PreconditionsUnavailable,
    DependenciesChanged,
    PolicyChanged,
    EvidenceInvalidated,
    SafetySignal,
    InsufficientEvidence,
    NotImproved,
}

/// Stored with the settlement checkpoint; recoverable from the immutable
/// plan and native verdict after a lost acknowledgement. No score is cached.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewSchedule {
    pub due_at: String,
    pub acquisition: AdoptionBasis,
    pub evaluation_ref: String,
    pub next_job_id: Option<String>,
    pub suspended: Option<ReviewReason>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewStatus {
    pub job_id: String,
    pub schedule: ReviewSchedule,
    pub due: bool,
    pub reasons: Vec<ReviewReason>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewPage {
    pub items: Vec<ReviewStatus>,
    pub next_after: u64,
    pub complete: bool,
}

/// A bounded current read, not a durable permission or a record of actual use.
/// `recommendation_allowed` expires at the earliest context/review deadline.
/// Every actual dispatch still needs its own Decision/Attempt and native gate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcedureStatus {
    pub skill_ref: String,
    pub revision_ref: String,
    pub lifecycle: String,
    pub validated_adoption: bool,
    pub recommendation_allowed: bool,
    pub authority_class: String,
    pub dependency_validity: Json,
    pub evaluation_ref: Option<String>,
    pub acquisition: Option<AdoptionBasis>,
    pub review_due_at: Option<String>,
    pub context_expires_at_ms: Option<u64>,
    pub reasons: Vec<String>,
    pub actual_use: String,
    pub execution_permission: String,
}

impl LearningRuntime {
    /// Binds a host-authenticated context observation. This grants neither
    /// lifecycle standing nor executable authority. It is intentionally volatile.
    pub fn bind_application_context(
        &self,
        auth: &AuthContext,
        context: Option<ApplicationContext>,
    ) -> Result<(), BoxError> {
        Self::require_host(auth)?;
        self.ensure_open()?;
        if let Some(c) = &context {
            let now = anda_engine::unix_ms();
            if !crate::learning::is_revision(&c.revision_ref)
                || !crate::learning::is_digest(&c.evidence_digest)
                || c.checked_at_ms > now
                || c.expires_at_ms <= now
                || c.expires_at_ms <= c.checked_at_ms
                || c.expires_at_ms - c.checked_at_ms > 30_000
            {
                return Err(
                    "application context requires a current, bounded host observation".into(),
                );
            }
        }
        *self.application_context.write() = context;
        Ok(())
    }

    /// Discover due reviews without changing standing. The host supplies fresh
    /// monitoring manifests through `enroll_review`; no timer invents tasks.
    pub async fn reviews(&self) -> Result<Vec<ReviewStatus>, BoxError> {
        let page = self.reviews_page(0, 32).await?;
        if !page.complete {
            return Err("learning review inventory exceeds one page; use reviews_page".into());
        }
        Ok(page.items)
    }
    /// Bounded archive-index walk plus the bounded hot set. The cursor reports
    /// discovery coverage only; enrollment is the separate durable obligation.
    pub async fn reviews_page(&self, after: u64, limit: usize) -> Result<ReviewPage, BoxError> {
        self.ensure_open()?;
        if !self.is_configured() {
            return Ok(ReviewPage {
                items: vec![],
                next_after: 0,
                complete: true,
            });
        }
        let reg = self.registration(false).await?;
        let context = self.application_context.read().clone();
        let mut result = vec![];
        let (ids, next_after, complete) = self.archived_review_ids(after, limit).await?;
        let mut rows = self.jobs().await?;
        for id in ids {
            if !rows.iter().any(|r| r.job_id == id) {
                rows.push(self.load_job(&reg, &id).await?.value.report());
            }
        }
        for row in rows {
            let Some(schedule) = row.review else { continue };
            // A linked follow-up owns its new schedule; old acquisition rows
            // remain immutable history, not repeated requests for the same job.
            if let Some(next) = &schedule.next_job_id
                && let Some(next) = self.journal.read::<Job>(&Self::key(next)?).await?
                && (next.value.stage != JobStage::Expired || next.value.activation_complete)
            {
                continue;
            }
            let job = self.load_job(&reg, &row.job_id).await?.value;
            let mut reasons = vec![];
            if self.clock.now_ms() >= time_ms(&schedule.due_at)? {
                reasons.push(ReviewReason::Deadline);
            }
            if let Some(reason) = &schedule.suspended {
                reasons.push(reason.clone());
            }
            if schedule.next_job_id.is_some() && reasons.is_empty() {
                reasons.push(ReviewReason::InsufficientEvidence);
            }
            if let Some(context) = &context
                && context.revision_ref == job.plan.candidate_revision
                && reg
                    .config
                    .validate_executor(&job.plan, &context.executor)
                    .is_err()
            {
                reasons.push(ReviewReason::EnvironmentChanged);
            }
            if let Some(context) = &context
                && context.revision_ref == job.plan.candidate_revision
                && (!context.preconditions_satisfied
                    || context.expires_at_ms <= anda_engine::unix_ms())
            {
                reasons.push(ReviewReason::PreconditionsUnavailable);
            }
            if self
                .native_for(&reg.config)
                .await?
                .validate_frozen(
                    &job.plan,
                    job.frozen.as_ref().ok_or("review policy missing")?,
                )
                .await
                .is_err()
            {
                reasons.push(ReviewReason::PolicyChanged);
            }
            if schedule.suspended.is_none()
                && self
                    .native_for(&reg.config)
                    .await?
                    .validate_current_verdict_evidence(
                        &Self::verdict_input(&reg, &job, "review-read")?,
                        &schedule.evaluation_ref,
                    )
                    .await
                    .is_err()
            {
                reasons.push(ReviewReason::EvidenceInvalidated);
            }
            let revision = self
                .read_one(format!(
                    "FIND(?r) WHERE {{?r CONCEPT {{id:{},type:\"SkillRevision\"}}}} LIMIT 1",
                    serde_json::to_string(&job.plan.candidate_revision)?
                ))
                .await?;
            if revision["_system"]["dependency_validity"]["action_eligible"] != true {
                reasons.push(ReviewReason::DependenciesChanged);
            }
            result.push(ReviewStatus {
                job_id: row.job_id,
                schedule,
                due: !reasons.is_empty(),
                reasons,
            });
        }
        Ok(ReviewPage {
            items: result,
            next_after,
            complete,
        })
    }

    /// Predeclare a new monitoring/re-entry cohort while retaining the original
    /// acquisition verdict. The durable link precedes enrollment, so retries
    /// recover the same follow-up job after either checkpoint is interrupted.
    pub async fn enroll_review(
        self: &Arc<Self>,
        source_job_id: String,
        job_id: String,
        plan: PairedTrialPlan,
        basis_proposition: String,
    ) -> Result<JobReport, BoxError> {
        self.enroll_review_origin(source_job_id, job_id, plan, basis_proposition, None)
            .await
    }
    pub(super) async fn enroll_review_origin(
        self: &Arc<Self>,
        source_job_id: String,
        job_id: String,
        plan: PairedTrialPlan,
        basis_proposition: String,
        origin: Option<super::super::EnrollmentOrigin>,
    ) -> Result<JobReport, BoxError> {
        self.owned(move |this| {
            Box::pin(async move {
                let _g = this.gate.lock().await;
                this.ensure_open()?;
                let reg = this.registration(true).await?;
                this.recover_catalog().await?;
                let mut source = this.load_job(&reg, &source_job_id).await?;
                if source_job_id == job_id || source.value.stage != JobStage::Settled {
                    return Err("review needs a new job after a settled source".into());
                }
                reg.config.validate_plan(&plan)?;
                let schedule = source
                    .value
                    .review
                    .as_ref()
                    .ok_or("source has no acquisition schedule")?;
                if plan.execution.review_of.as_ref() != Some(&schedule.acquisition)
                    || plan.candidate_revision != source.value.plan.candidate_revision
                    || plan.pin()? == source.value.plan.pin()?
                    || plan.execution.cutoff <= source.value.plan.execution.cutoff
                    || plan
                        .pairs
                        .values()
                        .any(|case| source.value.plan.pairs.values().any(|old| case == old))
                {
                    return Err(
                        "review must retain acquisition and predeclare a fresh later cohort".into(),
                    );
                }
                schedule.acquisition.validate_records(
                    &this
                        .record(&schedule.acquisition.trial_ref, "TrialRecord")
                        .await?,
                    &this
                        .record(&schedule.acquisition.evaluation_ref, "EvaluationRecord")
                        .await?,
                )?;
                let replace_failed = if let Some(old) = schedule.next_job_id.as_ref().filter(|old| *old != &job_id) {
                    // Only an authoritative NotFound proves that child creation
                    // never checkpointed: every native operation follows that
                    // checkpoint. Storage errors/uncertainty must propagate.
                    if this.journal.read::<Job>(&Self::key(old)?).await?.is_some() {
                        let previous = this.load_job(&reg, old).await?.value;
                        if previous.stage != JobStage::Expired || previous.activation_complete {
                            return Err("review already assigned to another persistent job".into());
                        }
                        if plan.execution.cutoff <= previous.plan.execution.cutoff
                            || plan.pairs.values().any(|case| previous.plan.pairs.values().any(|old| case == old))
                        {
                            return Err("replacement review needs a later fresh cohort; failed enrollment is retained".into());
                        }
                    }
                    true
                } else { false };
                // Invalid/expired context and exhausted capacity must not consume
                // the source's review obligation. The gate remains held through
                // preparation, the source CAS and the child's conditional create.
                let mut prepared = this.prepare_enrollment(&reg, &job_id, plan, basis_proposition).await?;
                this.bind_enrollment_origin(&reg, &job_id, &mut prepared, origin).await?;
                if schedule.next_job_id.is_none() || replace_failed {
                    source.value.review.as_mut().unwrap().next_job_id = Some(job_id.clone());
                    this.journal
                        .save(&Self::key(&source_job_id)?, &source)
                        .await?;
                }
                this.commit_enrollment(&reg, &job_id, prepared).await
            })
        })
        .await
    }

    /// Resolve one exact Skill through authenticated current KQL. Raw recall
    /// may still quote unproven/revoked content; only this result describes the
    /// configured controller's current recommendation eligibility.
    pub async fn procedure_status(&self, skill_ref: &str) -> Result<ProcedureStatus, BoxError> {
        self.ensure_open()?;
        // Do not wait behind a model callback, or combine native standing with
        // a concurrent private safety/review checkpoint. Unavailable is not a
        // validated recommendation; the caller may retry this bounded read.
        let _gate = self
            .gate
            .try_lock()
            .map_err(|_| "learning state is changing; recommendation is unverified")?;
        if !crate::learning::is_revision(skill_ref) {
            return Err("canonical Skill reference required".into());
        }
        let sequence = self.nexus.store.get_space(DEFAULT_SPACE).await?.seq;
        let skill = self
            .read_one(format!(
                "FIND(?s) WHERE {{?s CONCEPT {{id:{},type:\"Skill\"}}}} LIMIT 1",
                serde_json::to_string(skill_ref)?
            ))
            .await?;
        let revision_ref = edge(&skill, "current_revision")?.to_string();
        let revision = self
            .read_one(format!(
                "FIND(?r) WHERE {{?r CONCEPT {{id:{},type:\"SkillRevision\"}}}} LIMIT 1",
                serde_json::to_string(&revision_ref)?
            ))
            .await?;
        if edge(&revision, "revision_of")? != skill_ref {
            return Err("Skill and current revision disagree".into());
        }
        let grade = &skill["facets"][format!("{PROFILE}GradingState")];
        let trial_state = &skill["facets"][format!("{PROFILE}TrialState")];
        let mut result = ProcedureStatus {
            skill_ref: skill_ref.into(),
            revision_ref: revision_ref.clone(),
            lifecycle: skill["attributes"]["status"]
                .as_str()
                .unwrap_or("unknown")
                .into(),
            validated_adoption: false,
            recommendation_allowed: false,
            authority_class: revision["governance"]["authority_class"]
                .as_str()
                .unwrap_or("descriptive")
                .into(),
            dependency_validity: revision["_system"]["dependency_validity"].clone(),
            evaluation_ref: grade["evaluation_ref"].as_str().map(str::to_string),
            acquisition: None,
            review_due_at: None,
            context_expires_at_ms: None,
            reasons: vec![],
            actual_use:
                "not_established_by_recall; inspect revision-bound DecisionRecord/AttemptRecord"
                    .into(),
            execution_permission: "requires_fresh_native_dispatch_and_target_system_authorization"
                .into(),
        };
        if !self.is_configured() {
            result.reasons.push("learning_not_configured".into());
            return Ok(result);
        }
        let reg = self.registration(false).await?;
        if !reg.enabled {
            result.reasons.push("learning_disabled".into());
        }
        if result.lifecycle != "adopted" {
            result
                .reasons
                .push("revision_is_unproven_or_revoked".into());
            return Ok(result);
        }
        let mut reports = self.jobs().await?;
        if let Some(reference) = &result.evaluation_ref
            && let Some(id) = self.indexed_job("evaluations", reference).await?
            && !reports.iter().any(|r| r.job_id == id)
        {
            reports.push(self.load_job(&reg, &id).await?.value.report());
        }
        if self.pending_safety(&revision_ref).await? {
            result
                .reasons
                .push("safety_signal_requires_reconciliation".into());
        }
        for report in &reports {
            let other = self.load_job(&reg, &report.job_id).await?.value;
            if other.plan.candidate_revision == revision_ref
                && other
                    .safety
                    .as_ref()
                    .is_some_and(|s| s.evaluation_ref.is_none())
            {
                result
                    .reasons
                    .push("safety_signal_requires_reconciliation".into());
            }
        }
        let Some(report) = reports
            .iter()
            .find(|j| j.evaluation_ref.is_some() && j.evaluation_ref == result.evaluation_ref)
        else {
            result
                .reasons
                .push("no_recoverable_controller_verdict".into());
            return Ok(result);
        };
        let job = self.load_job(&reg, &report.job_id).await?.value;
        if job.safety.is_some() {
            result
                .reasons
                .push("safety_signal_pending_or_recorded".into());
        }
        let Some(review) = &job.review else {
            result.reasons.push("review_schedule_unavailable".into());
            return Ok(result);
        };
        let evaluation_ref = result.evaluation_ref.as_deref().ok_or("grade missing")?;
        let evaluation = self.record(evaluation_ref, "EvaluationRecord").await?;
        let trial_ref = job.trial_ref.as_deref().ok_or("trial missing")?;
        let trial = self.record(trial_ref, "TrialRecord").await?;
        if job.plan.candidate_revision != revision_ref
            || grade["revision_ref"] != revision_ref
            || trial_state["revision_ref"] != revision_ref
            || trial_state["trial_ref"] != trial_ref
            || evaluation["trial_ref"] != trial_ref
            || evaluation["revision_refs"] != json!([revision_ref])
            || evaluation["to_status"] != "adopted"
            || evaluation["comparison"]["status"] != "improved"
            || trial["parameters"] != json!(job.plan.pin()?)
            || evaluation["cutoff"] != job.plan.execution.cutoff
            || review.evaluation_ref != evaluation_ref
        {
            result
                .reasons
                .push("current_revision_verdict_binding_mismatch".into());
            return Ok(result);
        }
        review.acquisition.validate_records(
            &self
                .record(&review.acquisition.trial_ref, "TrialRecord")
                .await?,
            &self
                .record(&review.acquisition.evaluation_ref, "EvaluationRecord")
                .await?,
        )?;
        result.validated_adoption = true;
        result.acquisition = Some(review.acquisition.clone());
        result.review_due_at = Some(review.due_at.clone());
        if self
            .native_for(&reg.config)
            .await?
            .validate_current_verdict_evidence(
                &Self::verdict_input(&reg, &job, "applicability-read")?,
                evaluation_ref,
            )
            .await
            .is_err()
        {
            result.validated_adoption = false;
            result
                .reasons
                .push("adoption_evidence_changed_or_unavailable".into());
        }
        if self.clock.now_ms() >= time_ms(&review.due_at)? {
            result.reasons.push("review_due".into());
        }
        if let Some(reason) = &review.suspended {
            result.reasons.push(format!("review_suspended:{reason:?}"));
        }
        if self
            .native_for(&reg.config)
            .await?
            .validate_frozen(&job.plan, job.frozen.as_ref().ok_or("policy pin missing")?)
            .await
            .is_err()
        {
            result
                .reasons
                .push("evaluation_policy_changed_or_unavailable".into());
        }
        if result.dependency_validity["action_eligible"] != true {
            result
                .reasons
                .push("dependencies_not_action_eligible".into());
        }
        if !matches!(result.authority_class.as_str(), "advisory" | "executable") {
            result
                .reasons
                .push("revision_has_no_recommendation_authority".into());
        }
        let context = self.application_context.read().clone();
        let context_digest = content_digest(&json!(context))?;
        match context {
            Some(c)
                if c.revision_ref == revision_ref && c.expires_at_ms > anda_engine::unix_ms() =>
            {
                result.context_expires_at_ms = Some(c.expires_at_ms);
                if reg
                    .config
                    .validate_executor(&job.plan, &c.executor)
                    .is_err()
                {
                    result
                        .reasons
                        .push("environment_model_tools_memory_or_budget_changed".into());
                }
                if !c.preconditions_satisfied {
                    result
                        .reasons
                        .push("current_task_preconditions_not_satisfied".into());
                }
            }
            _ => result
                .reasons
                .push("current_application_context_unavailable".into()),
        }
        // A current read must not combine a previous grade with newly changed
        // source dependencies, policy or revision. Sequence changes fail closed.
        if self.nexus.store.get_space(DEFAULT_SPACE).await?.seq != sequence {
            result
                .reasons
                .push("native_state_changed_during_read".into());
        }
        if !self.registration(false).await?.enabled {
            result.reasons.push("learning_disabled".into());
        }
        if content_digest(&json!(*self.application_context.read()))? != context_digest {
            result
                .reasons
                .push("application_context_changed_during_read".into());
        }
        if self.clock.now_ms() >= time_ms(&review.due_at)?
            || result
                .context_expires_at_ms
                .is_none_or(|t| t <= anda_engine::unix_ms())
        {
            result.reasons.push("review_or_context_expired".into());
        }
        result.recommendation_allowed = result.reasons.is_empty();
        Ok(result)
    }

    pub(super) async fn record(&self, reference: &str, facet: &str) -> Result<Json, BoxError> {
        let row = self
            .read_one(format!(
                "FIND(?r) WHERE {{?r {} {{id:{}}}}} LIMIT 1",
                if reference.starts_with("E-") {
                    "EVIDENCE"
                } else {
                    "ACTIVITY"
                },
                serde_json::to_string(reference)?
            ))
            .await?;
        row["facets"]
            .get(format!("{PROFILE}{facet}"))
            .cloned()
            .ok_or_else(|| "native facet unavailable".into())
    }
}

pub(super) fn edge<'a>(value: &'a Json, name: &str) -> Result<&'a str, BoxError> {
    let refs = value["structural"]
        .get(format!("{PROFILE}{name}"))
        .and_then(Json::as_array)
        .filter(|r| r.len() == 1)
        .ok_or("one exact revision/family edge required")?;
    refs[0]
        .as_str()
        .or_else(|| refs[0]["id"].as_str())
        .ok_or_else(|| "revision/family reference missing".into())
}
