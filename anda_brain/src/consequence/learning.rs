use super::service::time_ms;
use super::*;
use crate::{
    learning::{LearningRuntime, ObservationRoute, OutcomeMeasurements, OutcomeSubmission},
    runtime_api::{RuntimeError, RuntimeResult, full_read},
};
use anda_cognitive_nexus::{content_digest, governance::AuthContext};
use serde_json::json;
use std::sync::Arc;

impl ConsequenceRuntime {
    pub(super) async fn submit_learning(
        &self,
        auth: &AuthContext,
        input: OutcomeInput,
        decision: String,
        learning: Arc<LearningRuntime>,
        route: ObservationRoute,
        now: u64,
    ) -> RuntimeResult<ObservationReceipt> {
        if input.space_instance != route.instance
            || input.observer_configuration_digest != route.observer_digest
            || input.metric != "success"
            || input.window != route.plan.observation_window
        {
            return Err(RuntimeError::Invalid(
                "observation differs from the registered learning contract".into(),
            ));
        }
        if !route.dispatched {
            return Err(RuntimeError::Invalid(
                "learning outcome cannot precede dispatch".into(),
            ));
        }
        let Observation::Learning { measurements } = &input.observation else {
            return Err(RuntimeError::Invalid(
                "registered learning Attempt requires its typed measurements".into(),
            ));
        };
        let measurements: OutcomeMeasurements = serde_json::from_value(measurements.clone())
            .map_err(|_| RuntimeError::Invalid("invalid learning measurement shape".into()))?;
        let submission = OutcomeSubmission {
            space_instance: route.instance.clone(),
            job_id: route.job_id,
            dispatch_id: route.dispatch_id,
            observer_configuration_digest: route.observer_digest.clone(),
            observation_key: input.event_key.clone(),
            observed_at: input.observed_at.clone(),
            measurements: measurements.clone(),
        };
        let native_digest = content_digest(&json!(submission))?;
        let recovery = route.replay_digests.contains(&native_digest);
        let scope = RuntimeScope {
            space_id: self.scope.space_id.clone(),
            space_instance: route.instance,
        };
        let digest = content_digest(&json!(input))?;
        let storage_key = self.receipt_key(&scope, &auth.principal_id, &input.event_key)?;
        let mut stored =
            if let Some(old) = self.directory.read::<StoredReceipt>(&storage_key).await? {
                let old = old.value;
                self.validate_stored(&old, &scope, &auth.principal_id)?;
                if old.receipt.body_digest != digest {
                    return self.conflict(&old, &input, &digest).await;
                }
                if old.receipt.status.ends_with("_audit") {
                    return Ok(old.receipt);
                }
                old
            } else {
                let mut row = self.prepare_receipt(
                    &scope,
                    auth,
                    &input,
                    &digest,
                    decision,
                    None,
                    route.observer_digest,
                    now,
                )?;
                if input.correction_of.is_some() || (route.has_outcome && !recovery) {
                    row.receipt.status = "conflict_audit".into();
                    row.receipt.reason = Some(
                        "learning terminal evidence is immutable; correction retained for review"
                            .into(),
                    );
                } else if !recovery
                    && (route.late_now
                        || time_ms(&input.observed_at)? > time_ms(&route.plan.execution.cutoff)?)
                {
                    row.receipt.status = "late_audit".into();
                    row.receipt.reason =
                        Some("outside the frozen learning cutoff; not added to the trial".into());
                } else if !measurements.finished {
                    row.receipt.status = "progress_audit".into();
                    row.receipt.reason =
                        Some("nonterminal progress cannot close a learning attempt".into());
                }
                self.save(&storage_key, &row).await?;
                self.index(&row).await?;
                row
            };
        if stored.receipt.status.ends_with("_audit") {
            return Ok(stored.receipt);
        }
        // The existing controller alone owns native Evidence and its journal.
        // This also preserves its exact native-ACK recovery before cutoff checks.
        let report = match learning
            .submit_outcome(auth.clone(), submission.clone())
            .await
        {
            Ok(report) => report,
            Err(e) if e.downcast_ref::<crate::learning::LateOutcome>().is_some() => {
                stored.receipt.status = "late_audit".into();
                stored.receipt.reason =
                    Some("frozen learning cutoff reached before native acceptance".into());
                self.save(&storage_key, &stored).await?;
                self.index(&stored).await?;
                return Ok(stored.receipt);
            }
            Err(e) => return Err(RuntimeError::Storage(e)),
        };
        let attempt = report
            .attempts
            .iter()
            .find(|a| a.dispatch_id == submission.dispatch_id)
            .ok_or_else(|| {
                RuntimeError::Unavailable("learning receipt does not name its Attempt".into())
            })?;
        let reference=attempt.outcome_ref.clone().ok_or_else(||RuntimeError::Unavailable("learning observation commit unresolved; retry identical request with fresh authentication".into()))?;
        full_read(&self.nexus.session(auth.clone()), &reference).await?;
        stored.receipt.outcome_ref = Some(reference);
        stored.receipt.native_committed = true;
        stored.receipt.learning_eligible = true;
        stored.receipt.status = "learning_committed".into();
        let native_status = measurements
            .classify(&route.plan)
            .map_err(RuntimeError::Storage)?;
        stored.receipt.outcome_status = Some(
            serde_json::from_value(json!(native_status))
                .map_err(|e| RuntimeError::Storage(e.into()))?,
        );
        self.save(&storage_key, &stored).await?;
        self.index(&stored).await?;
        Ok(stored.receipt)
    }
}
