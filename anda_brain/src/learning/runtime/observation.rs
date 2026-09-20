//! Read-only routing for the general observation ingress. Membership comes from
//! the retained job/ticket, including baseline attempts whose trial_ref is null.
use super::*;

#[derive(Debug)]
pub(crate) struct LateOutcome;
impl std::fmt::Display for LateOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("late outcome is not eligible; original attempt remains unobserved")
    }
}
impl std::error::Error for LateOutcome {}

pub(crate) struct ObservationRoute {
    pub instance: String,
    pub job_id: String,
    pub dispatch_id: String,
    pub observer_digest: String,
    pub plan: PairedTrialPlan,
    pub late_now: bool,
    pub dispatched: bool,
    pub has_outcome: bool,
    pub replay_digests: Vec<String>,
}
impl LearningRuntime {
    pub(crate) async fn observation_instance(&self) -> Result<String, BoxError> {
        Ok(self.registration(false).await?.instance)
    }
    pub(crate) async fn observation_route(
        &self,
        attempt_ref: &str,
        auth: &AuthContext,
    ) -> Result<Option<ObservationRoute>, BoxError> {
        if !self.is_configured() {
            return Ok(None);
        }
        let _g = self.gate.lock().await;
        self.ensure_open()?;
        let reg = self.registration(false).await?;
        let mut keys = self.hot_keys().await?;
        if let Some(id) = self.indexed_job("attempts", attempt_ref).await? {
            let key = Self::key(&id)?;
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        for key in keys {
            let raw = self
                .journal
                .read::<Job>(&key)
                .await?
                .ok_or("learning job disappeared")?;
            let state = self.load_job(&reg, &raw.value.id).await?;
            if let Some((key, a)) = state
                .value
                .attempts
                .iter()
                .find(|(_, a)| a.ticket.attempt_ref == attempt_ref)
            {
                if auth.principal_id != reg.config.observer.principal_id
                    || auth.auth_method.is_empty()
                    || auth.auth_strength == "none"
                    || !auth.delegation_chain.is_empty()
                {
                    return Err("direct registered learning observer required".into());
                }
                let mut replay_digests: Vec<String> = a.receipt_digest.iter().cloned().collect();
                if let Some(p) = &state.value.pending
                    && p.attempt_key.as_ref() == Some(key)
                {
                    replay_digests.extend(p.receipt_digest.iter().cloned());
                }
                return Ok(Some(ObservationRoute {
                    instance: reg.instance.clone(),
                    job_id: state.value.id.clone(),
                    dispatch_id: a.ticket.dispatch_id.clone(),
                    observer_digest: reg.config.observer.configuration_digest.clone(),
                    late_now: self.clock.now_ms() > time_ms(&state.value.plan.execution.cutoff)?
                        || state.value.stage == JobStage::Expired,
                    dispatched: !matches!(
                        a.state,
                        DispatchState::Prepared | DispatchState::Authorizing
                    ),
                    has_outcome: a.outcome_ref.is_some(),
                    plan: state.value.plan.clone(),
                    replay_digests,
                }));
            }
        }
        Ok(None)
    }
}
