//! Fixed native requests and target lookup; delivery status is not an Outcome.
use super::journal::Job;
use super::*;
use anda_cognitive_nexus::{
    attention::{DispatchLookupStatus, WakeState},
    content_digest,
    nexus::{DEFAULT_SPACE, Session},
};
use serde_json::json;

impl ActionRuntime {
    pub(super) async fn dispatch(
        &self,
        session: &Session,
        wake: &mut WakeRecord,
        job: &mut Job,
        parent: &Job,
    ) -> Result<(), BoxError> {
        let prepared = parent
            .prepared
            .as_ref()
            .ok_or("dispatch preparation missing")?;
        let request = prepared
            .request
            .as_ref()
            .ok_or("dispatch has no fixed request")?;
        let attempt = parent
            .status
            .attempt_ref
            .as_deref()
            .ok_or("dispatch has no committed attempt")?;
        let attempt_row =
            context::element(session, attempt, None, self.bindings.limits.callbacks_ms).await?;
        let record = &attempt_row["facets"][format!("{PROFILE}AttemptRecord")];
        let selection = serde_json::from_value(record["selection_policy"].clone())?;
        let material = session.read_artifact(DEFAULT_SPACE, &selection).await?;
        if record["attempt_id"] != request.attempt_id
            || record["context"]["request_digest"] != content_digest(&json!(request))?
            || material["request"] != json!(request)
            || material["configuration"] != self.bindings.manifest()
        {
            return Err("dispatch journal differs from the native committed request".into());
        }
        job.status.decision_ref = parent.status.decision_ref.clone();
        job.status.attempt_ref = Some(attempt.into());
        let mut prior = None;
        if let Some(reference) = &job.status.dispatch_ref {
            prior = session.read_control(DEFAULT_SPACE, reference, None).await?;
        }
        // Lost directory ACK: native identity is derivable from the committed
        // attempt, so recovery discovers the already-started dispatch first.
        if prior.is_none() {
            let reference = dispatch_reference(&wake.scope, &request.attempt_id)?;
            prior = session
                .read_control(DEFAULT_SPACE, &reference, None)
                .await?;
            if prior.is_some() {
                job.status.dispatch_ref = Some(reference);
            }
        }
        if request.kind == ActionKind::DeliverClarification
            && self
                .answered(
                    &wake.scope,
                    request.payload["correlation"]
                        .as_str()
                        .ok_or("clarification correlation missing")?,
                    request.payload["recipient"]
                        .as_str()
                        .ok_or("clarification recipient missing")?,
                    request.payload["reply_deadline_ms"]
                        .as_u64()
                        .ok_or("clarification deadline missing")?,
                )
                .await?
            && (prior.is_none() || prior.as_ref().is_some_and(|r| r.value["state"] == "ready"))
        {
            session
                .cancel_wake(
                    DEFAULT_SPACE,
                    &wake.wake_ref,
                    wake.version,
                    wake.fence,
                    "clarification answered before delivery",
                )
                .await?;
            job.status.state = "cancelled".into();
            job.status.reason = Some("clarification_answered_before_delivery".into());
            job.status.next_run_ms = None;
            return Ok(());
        }
        if prior
            .as_ref()
            .is_some_and(|r| r.value["state"] == "completed")
        {
            self.acquire(session, wake, job).await?;
            session
                .finish_wake(
                    DEFAULT_SPACE,
                    &wake.wake_ref,
                    wake.version,
                    wake.fence,
                    "",
                    Default::default(),
                    vec![],
                )
                .await?;
            job.status.state = "completed".into();
            job.status.next_run_ms = None;
            return Ok(());
        }
        if job.failures > self.bindings.limits.max_retries
            && job.status.state == "blocked"
            && !prior.as_ref().is_some_and(|r| r.value["state"] == "ready")
        {
            return Ok(());
        }
        if let Some(old) = prior.as_ref().filter(|r| r.value["state"] != "ready") {
            if let Some(lookup) = &self.bindings.lookup
                && job.status.attempts <= self.bindings.limits.max_retries
                && !matches!(job.delivery, Some(DeliveryStatus::Finished))
            {
                let reference = job
                    .status
                    .dispatch_ref
                    .clone()
                    .ok_or("dispatch reference missing")?;
                job.status.attempts += 1;
                self.save(job).await?;
                let (auth, observation) = self
                    .bounded(lookup.client.lookup(request, &reference))
                    .await?;
                let state = observation.status;
                self.nexus
                    .session(auth)
                    .reconcile_wake_lookup(DEFAULT_SPACE, &reference, old.version, observation)
                    .await?;
                job.status.next_run_ms =
                    Some(anda_engine::unix_ms() + self.bindings.limits.retry_ms);
                match state {
                    DispatchLookupStatus::NotStarted => {
                        job.delivery = None;
                        job.status.state = "ready".into();
                    }
                    DispatchLookupStatus::Finished => {
                        job.delivery = Some(DeliveryStatus::Finished);
                        job.status.state = "awaiting_outcome".into();
                    }
                    DispatchLookupStatus::Running => {
                        job.delivery = Some(DeliveryStatus::Accepted);
                        job.status.state = "awaiting_outcome".into();
                    }
                    DispatchLookupStatus::Unknown => {
                        job.status.state = "outcome_unknown".into();
                    }
                }
                return Ok(());
            }
            let can_replay = self.executor(&request.kind)?.supports_idempotency()
                && job
                    .delivery
                    .is_none_or(|s| matches!(s, DeliveryStatus::Unknown))
                && job.status.attempts <= self.bindings.limits.max_retries
                && old.value["state"] != "outcome_unknown";
            if !can_replay {
                job.status.reason =
                    Some("dispatch_requires_independent_outcome_or_authoritative_lookup".into());
                if !matches!(wake.state, WakeState::Blocked { .. }) {
                    self.acquire(session, wake, job).await?;
                    self.block(session, wake, job, "outcome_unknown").await?;
                } else {
                    job.status.state = "blocked".into();
                    job.status.next_run_ms = None;
                }
                return Ok(());
            }
        }
        if job.status.attempts > self.bindings.limits.max_retries {
            job.status.reason = Some("dispatch_retry_budget_exhausted".into());
            if !matches!(wake.state, WakeState::Blocked { .. }) {
                self.acquire(session, wake, job).await?;
                self.block(session, wake, job, "budget_exhausted").await?;
            }
            return Ok(());
        }
        self.acquire(session, wake, job).await?;
        self.authorize(request).await?;
        self.bounded(prepared.capture.revalidate(
            session,
            wake,
            &self.bindings.limits,
            request.kind == ActionKind::DeliverClarification,
        ))
        .await?;
        job.status.attempts += 1;
        self.save(job).await?;
        // Refresh principal authentication after all callbacks, then perform the
        // native check as the last awaited step before actual external I/O.
        let fresh = self.session(&wake.scope).await?;
        let executor = self.executor(&request.kind)?;
        let permit = fresh
            .begin_wake_dispatch(
                DEFAULT_SPACE,
                &wake.wake_ref,
                wake.version,
                wake.fence,
                attempt,
                executor.supports_idempotency(),
                self.bindings.lookup.is_some(),
            )
            .await?;
        let reference = permit["dispatch_ref"]
            .as_str()
            .ok_or("native dispatch reference missing")?
            .to_string();
        job.status.dispatch_ref = Some(reference.clone());
        if permit["idempotency_key"] != request.attempt_id {
            return Err("native attempt identity changed".into());
        }
        match permit["action"].as_str() {
            Some("dispatch") => {
                let expires_at_ms = match &wake.state {
                    WakeState::Running { lease } => lease.expires_at_ms,
                    _ => return Err("dispatch lacks live lease".into()),
                };
                let p = DispatchPermit {
                    wake_ref: wake.wake_ref.clone(),
                    attempt_ref: attempt.into(),
                    dispatch_ref: reference,
                    fence: wake.fence,
                    expires_at_ms,
                };
                // No directory write between native admission and the callback.
                // If ACK is lost, begin's durable intent forces lookup/replay.
                job.delivery = Some(
                    self.bounded(executor.dispatch(request, &p))
                        .await
                        .unwrap_or(DeliveryStatus::Unknown),
                );
                job.status.state = if matches!(job.delivery, Some(DeliveryStatus::Unknown)) {
                    "outcome_unknown"
                } else {
                    "awaiting_outcome"
                }
                .into();
            }
            Some("lookup") | Some("outcome_unknown") => job.status.state = "outcome_unknown".into(),
            Some("done") => job.status.state = "awaiting_completion".into(),
            _ => return Err("unsupported native dispatch result".into()),
        }
        job.status.next_run_ms = Some(anda_engine::unix_ms() + self.bindings.limits.retry_ms);
        Ok(())
    }
}
