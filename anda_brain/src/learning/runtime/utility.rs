//! Utility reuses the registered native comparison; no new trial evaluator.
use super::*;
use crate::consequence::{AttributionSample, UtilityConfig};
impl LearningRuntime {
    pub(crate) async fn utility_sample(
        &self,
        evaluation: &str,
        config: &UtilityConfig,
    ) -> Result<AttributionSample, BoxError> {
        let _g = self.gate.lock().await;
        let reg = self.registration(false).await?;
        let id = if let Some(id) = self.indexed_job("evaluations", evaluation).await? {
            id
        } else {
            self.jobs()
                .await?
                .into_iter()
                .find(|j| j.evaluation_ref.as_deref() == Some(evaluation))
                .ok_or("native evaluation has no registered controller job")?
                .job_id
        };
        let job = self.load_job(&reg, &id).await?.value;
        let record = self.record(evaluation, "EvaluationRecord").await?;
        let row = crate::runtime_api::full_read(
            &self.nexus.system_session(),
            &job.plan.candidate_revision,
        )
        .await?;
        let mut digests = BTreeMap::new();
        let references: Vec<String> = std::iter::once(evaluation)
            .chain(job.trial_ref.as_deref())
            .chain(job.attempts.values().flat_map(|a| {
                std::iter::once(a.ticket.attempt_ref.as_str()).chain(a.outcome_ref.as_deref())
            }))
            .map(str::to_string)
            .collect();
        for reference in references {
            if digests.len() >= 128 {
                return Err("paired utility evidence budget exceeded".into());
            }
            let view =
                crate::runtime_api::full_read(&self.nexus.system_session(), &reference).await?;
            digests.insert(
                reference.to_string(),
                crate::recall_receipt::semantic_digest(&view)?,
            );
        }
        let comparison = &record["comparison"];
        let effect = comparison["effect"].as_f64();
        let lower = comparison["uncertainty"]["lower_bound"].as_f64();
        let upper = effect
            .zip(comparison["uncertainty"]["radius"].as_f64())
            .map(|(v, r)| (v + r).min(1.0));
        let reason = if job.safety.is_some()
            || job.stage != JobStage::Settled
            || job.evaluation_ref.as_deref() != Some(evaluation)
        {
            Some("not_a_current_comparison_verdict")
        } else if config.task_family != job.plan.task_family
            || config.environment_digest != job.plan.environment_digest
            || config.tool_versions != job.plan.tool_versions
            || config.metric != "success"
            || config.window != job.plan.observation_window
            || json!(config.observer) != json!(reg.config.observer)
        {
            Some("outside_comparable_scope")
        } else if effect.is_none() || lower.is_none() || upper.is_none() {
            Some("insufficient_native_comparison")
        } else if self
            .native_for(&reg.config)
            .await?
            .validate_current_verdict_evidence(
                &Self::verdict_input(&reg, &job, "utility-read")?,
                evaluation,
            )
            .await
            .is_err()
        {
            Some("native_comparison_evidence_invalidated")
        } else {
            None
        };
        Ok(AttributionSample {
            outcome_ref: evaluation.into(),
            attempt_ref: job.trial_ref.clone().unwrap_or_default(),
            decision_ref: evaluation.into(),
            target: crate::recall_receipt::pin(&row)?,
            used_refs: vec![job.plan.candidate_revision.clone()],
            applied_revisions: vec![job.plan.candidate_revision.clone()],
            recall_receipt: None,
            evidence_refs: digests.keys().cloned().collect(),
            evidence_digests: digests,
            independent_unit: content_digest(&json!([
                reg.instance,
                job.plan.candidate_revision,
                job.plan.pairs
            ]))?,
            effect,
            lower_bound: lower.map(|v| v.max(-1.0)),
            upper_bound: upper,
            confidence: Some((1.0 - 2.0 * job.plan.alpha).max(0.0)),
            independent_samples: job.plan.pairs.len(),
            reason: reason.map(str::to_string),
        })
    }
}
