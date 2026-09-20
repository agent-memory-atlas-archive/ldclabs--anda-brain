use super::*;
use crate::runtime_api::{PROFILE, RuntimeError, RuntimeResult, full_read};
use anda_cognitive_nexus::{governance::AuthContext, nexus::DEFAULT_SPACE};
use serde_json::json;

pub(super) fn command(
    input: &OutcomeInput,
    decision: &str,
    correction_ref: Option<&str>,
    task_family: &str,
) -> RuntimeResult<String> {
    let Observation::Measurement {
        terminal,
        outcome_status,
        magnitude,
        ..
    } = &input.observation
    else {
        return Err(RuntimeError::Invalid(
            "learning measurements require their registered controller".into(),
        ));
    };
    let facet = json!({"task_family":task_family,"attempt_ref":input.attempt_ref,"metric":input.metric,"window":input.window,"terminal":terminal,
        "observation_key":input.event_key,"observer_config_digest":input.observer_configuration_digest,"outcome_status":outcome_status,"magnitude":magnitude});
    let payload = crate::kip::string_literal(
        &serde_json::to_string(input).map_err(|e| RuntimeError::Storage(e.into()))?,
    );
    let correction = correction_ref
        .map(|id| format!(" (\"inputs\",{})", crate::kip::string_literal(id)))
        .unwrap_or_default();
    Ok(format!(
        r#"MUTATE {{
      CREATE EVIDENCE ?outcome {{SET FIELDS {{evidence_class:"outcome",payload:{payload},observed_at:{}}} SET FACET "OutcomeRecord" {facet}}}
      CREATE ACTIVITY ?observation {{SET FIELDS {{activity_class:"outcome_observation",status:"completed"}} SET STRUCTURAL {{("inputs",{}) ("inputs",{}) ("outputs",?outcome){correction}}}}}
    }}"#,
        crate::kip::string_literal(&input.observed_at),
        crate::kip::string_literal(decision),
        crate::kip::string_literal(&input.attempt_ref)
    ))
}

impl ConsequenceRuntime {
    pub(super) async fn commit_native(
        &self,
        auth: &AuthContext,
        stored: &mut StoredReceipt,
    ) -> RuntimeResult<()> {
        let session = self.nexus.session(auth.clone());
        let mut request = crate::kip::request(
            stored
                .command
                .clone()
                .ok_or_else(|| RuntimeError::Invalid("observation preparation missing".into()))?,
        );
        request.operations[0].idempotency_key = Some(stored.native_key.clone());
        request.parameters = Some(crate::kip::param(
            "brain_observation_receipt_digest",
            stored.receipt.body_digest.clone(),
        ));
        let response = anda_kip::execute_request(&session, &request).await;
        if let Some(error) = crate::kip::error_of(&response) {
            return Err(anda_kip::KipError::new(
                error
                    .parsed_code()
                    .unwrap_or(anda_kip::KipErrorCode::InternalError),
                error.message.clone(),
            )
            .into());
        }
        let result=crate::kip::ok_result(&response).ok_or_else(||RuntimeError::Unavailable("observation commit outcome unknown; retry the identical request with fresh authentication".into()))?;
        let outcome = result["handles"]["outcome"]
            .as_str()
            .ok_or_else(|| {
                RuntimeError::Unavailable("native observation receipt incomplete".into())
            })?
            .to_string();
        let observation = result["handles"]["observation"]
            .as_str()
            .ok_or_else(|| {
                RuntimeError::Unavailable("native observation receipt incomplete".into())
            })?
            .to_string();
        let view = full_read(&session, &outcome).await?;
        if view["facets"][format!("{PROFILE}OutcomeRecord")]["attempt_ref"]
            != stored.input.attempt_ref
        {
            return Err(RuntimeError::Conflict(
                "native observation receipt identity mismatch".into(),
            ));
        }
        stored.receipt.native_committed = true;
        stored.receipt.outcome_ref = Some(outcome);
        stored.receipt.observation_ref = Some(observation);
        stored.receipt.status = if stored.input.correction_of.is_some() {
            "correction_recorded"
        } else {
            "committed"
        }
        .into();
        Ok(())
    }

    pub(super) async fn reconcile(
        &self,
        auth: &AuthContext,
        stored: &StoredReceipt,
    ) -> RuntimeResult<()> {
        let Observation::Measurement {
            terminal: true,
            outcome_status,
            ..
        } = &stored.input.observation
        else {
            return Ok(());
        };
        if *outcome_status == OutcomeStatus::Unknown || stored.input.correction_of.is_some() {
            return Ok(());
        }
        let (Some(reference), Some(outcome)) = (&stored.dispatch_ref, &stored.receipt.outcome_ref)
        else {
            return Ok(());
        };
        let session = self.nexus.session(auth.clone());
        let row = self
            .nexus
            .system_session()
            .read_control(DEFAULT_SPACE, reference, None)
            .await?
            .ok_or(RuntimeError::NotFound)?;
        if row.value["outcome_ref"].is_string() && row.value["outcome_ref"] != *outcome {
            return Err(RuntimeError::Conflict(
                "dispatch already has a different terminal observation".into(),
            ));
        }
        session
            .reconcile_wake_dispatch(DEFAULT_SPACE, reference, row.version, outcome)
            .await?;
        Ok(())
    }
}
