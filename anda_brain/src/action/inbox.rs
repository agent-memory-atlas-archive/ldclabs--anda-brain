//! Read-only projection for the product inbox. No claim, replay mutation, model
//! call or directory repair is performed by a read.
use super::*;
use crate::runtime_api::{RuntimeError, RuntimeResult, full_read};
use anda_cognitive_nexus::{
    attention::{WakeRecord, WakeState},
    nexus::DEFAULT_SPACE,
};

#[derive(Default)]
pub(crate) struct Inspection {
    pub status: Option<ActionStatus>,
    pub recipient: Option<String>,
    pub clarification: Option<Json>,
    pub decision: Option<Json>,
    pub request: Option<ActionRequest>,
}

impl ActionRuntime {
    pub(crate) async fn inspect(
        &self,
        wake: &WakeRecord,
        auth: &AuthContext,
    ) -> RuntimeResult<Inspection> {
        let session = self.nexus.session(auth.clone());
        let owner = if wake.continuation_key.as_deref() == Some("dispatch") {
            wake.parent_ref.as_deref().ok_or(RuntimeError::NotFound)?
        } else {
            &wake.wake_ref
        };
        let Some(job) = self.load(&wake.scope, owner).await? else {
            return Ok(Inspection::default());
        };
        if job.pins != wake.pins {
            return Err(RuntimeError::NotFound);
        }
        let Some(p) = &job.prepared else {
            return Ok(Inspection {
                status: Some(job.status),
                ..Default::default()
            });
        };
        // A proposal can have depended on native support/counter-evidence in
        // its packet, not just the top-level retrieved Proposition IDs.
        let mut sources = std::collections::BTreeSet::new();
        let packet =
            serde_json::to_value(&p.capture.packet).map_err(|e| RuntimeError::Storage(e.into()))?;
        let mut stack = vec![&packet];
        let mut nodes = 0;
        while let Some(value) = stack.pop() {
            nodes += 1;
            if nodes > 16_384 || sources.len() > 256 {
                return Err(RuntimeError::Unavailable(
                    "inbox visibility verification exceeds its budget".into(),
                ));
            }
            match value {
                Json::String(s) if s.parse::<anda_cognitive_nexus::ElementId>().is_ok() => {
                    sources.insert(s.clone());
                }
                Json::Object(m) => stack.extend(m.values()),
                Json::Array(a) => stack.extend(a),
                _ => {}
            }
        }
        sources.extend(p.capture.retrieved.iter().cloned());
        for reference in sources {
            full_read(&session, &reference).await?;
        }
        let mut status = job.status;
        let parent = self
            .nexus
            .system_session()
            .read_wake(DEFAULT_SPACE, owner)
            .await?;
        let mut outputs = job.outputs;
        if let WakeState::Completed { receipt_ref } = &parent.state {
            // Versioned control receipt, used only as a host discovery index.
            // All graph records are still read through the caller's authority.
            let row = self
                .nexus
                .store
                .control_at(DEFAULT_SPACE, receipt_ref, u64::MAX)
                .await?
                .ok_or(RuntimeError::NotFound)?;
            if row.value["format"] != crate::attention::FORMAT
                || row.value["identity"]["scope"] != serde_json::json!(wake.scope)
                || row.value["state"]["status"] != "committed"
            {
                return Err(RuntimeError::NotFound);
            }
            outputs = row.value["state"]["outputs"]
                .as_array()
                .ok_or(RuntimeError::NotFound)?
                .iter()
                .filter_map(|r| r.as_str().map(String::from))
                .collect();
            status.state = "committed".into();
            status.decision = Some(p.decision.clone());
        }
        let mut decision = None;
        for reference in outputs.into_iter().filter(|r| r.starts_with("X-")) {
            let row = full_read(&session, &reference).await?;
            let d = &row["facets"][format!("{PROFILE}DecisionRecord")];
            if d["decision"] == p.decision {
                status.decision_ref = Some(reference.clone());
                decision = Some(d.clone());
            }
            if p.request.as_ref().is_some_and(|r| {
                row["facets"][format!("{PROFILE}AttemptRecord")]["attempt_id"] == r.attempt_id
            }) {
                let pin = serde_json::from_value(
                    row["facets"][format!("{PROFILE}AttemptRecord")]["selection_policy"].clone(),
                )
                .map_err(|e| RuntimeError::Storage(e.into()))?;
                let material = session.read_artifact(DEFAULT_SPACE, &pin).await?;
                if material["request"] != serde_json::json!(p.request.as_ref().unwrap()) {
                    return Err(RuntimeError::NotFound);
                }
                status.attempt_ref = Some(reference);
            }
        }
        let recipient = p.recipient.clone().or_else(|| {
            p.request
                .as_ref()
                .filter(|r| r.kind == ActionKind::DeliverClarification)
                .and_then(|r| r.payload["recipient"].as_str().map(String::from))
        });
        let clarification = if matches!(parent.state, WakeState::Completed { .. }) {
            p.clarification_payload.clone()
        } else {
            None
        };
        if let Some(request) = &p.request {
            status.dispatch_ref = Some(dispatch_reference(&request.scope, &request.attempt_id)?);
        }
        Ok(Inspection {
            status: Some(status),
            recipient,
            clarification,
            decision,
            request: p.request.clone(),
        })
    }
}
